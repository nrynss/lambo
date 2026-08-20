# Adversarial review — lambo-for-mooshik workstream J, task J1, round 3

- **Reviewer:** `j1-reviewer-r3` (independent — I wrote nothing under review and neither of the two prior reviews. Every claim below is re-derived at the artifact rather than read out of a commit message. Verification-only edits: **six**, all in `src/mcp/server.rs` — four probe-test insertions and two mutations (`breaks_one_line` neutered to the round-1 literals; `conflict_err`'s two selections reverted to the `Conflict` variant) — reverted from a byte copy taken before the first and verified by `md5 -q` (`1f55f6b2ea1dddecd905fe51109d5b74`); plus one `git checkout 8963b2e -- src/` for the parent test baseline, restored with `git checkout f083a5a -- src/`. `src/cli/caps.rs` (`8afc3713…`) and `src/graph/reserve.rs` (`8ee494bd…`) were never edited; both md5s re-confirmed at the end. `git status --porcelain` clean but for this file at every checkpoint. Nothing but this file is committed; no commit is amended.)
- **Scope:** `f083a5a` ("fix(serve): J1 round-2 remediation — line-separator class, the lease-lost disclosure, two residual placements") against `8963b2e`. **8 files, +496/−71**: `src/mcp/server.rs` (+287/−…), `dev-diary/lambo-for-mooshik/J-multi-client.md` (+156), `src/memory.rs`, `src/types/mod.rs`, `src/graph/reserve.rs`, `src/cli/caps.rs` (+28, comment-only), `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx`. Authorities read: `adve-review-mooshik-J1-round2.md` in full (the checklist — J1-R2-1..4 and their prescriptions), `adve-review-mooshik-J1-round1.md` for history, `J-multi-client.md` §J1 including the new round-2 subsection, §J2, §Guidance and the Done-when board, `adve-review-mooshik-I-round3.md` for the carryover precedent.
- **Verdict:** **CLEAN** — three **P3** advisory findings (**J1-R3-1**, **J1-R3-2**, **J1-R3-3**), none blocking, none requiring a fourth round.

All four round-2 findings are closed at the artifact, and both blockers are closed **at the mechanism** rather than at the instance. I could not break either conclusion, and I attacked both from the direction the remediation itself invites.

The `breaks_one_line` split is the better of the two fixes, because it is the one that could most easily have been cosmetic and is not. One predicate now answers "does this character forge a line or a column", and *both* users consult it — the door and the fold, which round 2 found disagreeing. Neutering it back to the round-1 three literals turns both committed tests red **and** reproduces the round-2 defect end to end: a `U+2028` id is accepted, becomes the soft-lock holder, and lands verbatim — Lambo's own `⚑ CANONICAL` vocabulary and all — in an innocent agent's context block. At HEAD all seven tools refuse it, by the *line* rule specifically (the refusal names `agent_id`, says "single line", and names the codepoint), and `check_size` still passes it, so the door is genuinely load-bearing rather than shadowed by an upstream check. I also checked the two category claims the docstring rests on and both are true: `char::is_control()` is `Cc` exactly (empirically, across 37 codepoints — every C0/C1 control true, nothing else), and `Zl`/`Zp` really are singleton categories.

The `SoftLock` split is the one that had to be right, and it is, in the direction that matters. `LamboError::SoftLock` is constructed in exactly **two** places tree-wide — `src/graph/reserve.rs:120` and `:154` — so "produced by `graph::reserve` and nowhere else" is not a claim about intent, it is a fact about the crate. Reverting `conflict_err`'s two selections to `LamboError::Conflict` reproduces the round-2 leak verbatim, `DELETE FROM session_leases …` and all, and turns **two** tests red, not one: the new disclosure test *and* `two_agents_through_one_server_hold_distinct_locks`. That second failure is the interesting one — it means the split is pinned from both sides, so a future author cannot quietly re-widen the exception without also breaking the test that proves the exception is still doing its job.

The three advisories are register precision and one line of prose width. The load-bearing one is that `dev-diary/PHASE-2-graph-core.md` — the document that *specified* `graph::reserve`'s error contract — still says that contract is `LamboError::Conflict`, which this commit made false. The remediation's own sweep list names the `Conflict` doc references "under `memory.rs`/`graph::reserve`", i.e. the source comments, and stops there.

## Method

1. `lambo_recall` "J1 round 2 SoftLock breaks_one_line" (`j1-reviewer-r3`) — 5 hits: the J1-R2-1 constraint in full, the `breaks_one_line` resource node, the err_class/`error_kind` invariant, the J1 identity decision, and the round-2 verdict resource. The pinned dogfood rig still emits the deleted attribution warning on every call; that is the pinned binary talking, not evidence about this commit.
2. Read the round-2 review in full first (all four prescriptions), then `git show f083a5a`, non-test hunks before test hunks, then every claim at the source.
3. Attacked `breaks_one_line` at its own source with a 37-codepoint truth table that prints, for each character, `is_control()`, `breaks_one_line()`, and which of the two layers refuses it — so an "accepted" verdict is attributable rather than inferred.
4. Traced every constructor of both variants tree-wide, then every match arm on `LamboError`, then each downstream consumer the brief named: `err_class`, `map_writer_err`, `From<LamboError> for CliError` + `exit_code`, the daemon, `serve_web`, the ledger sample.
5. Mutation-tested both new tests in the two directions the brief named, and read the mutated output rather than only the pass/fail.
6. Re-derived the eviction measurement independently, and drove `simulate_lease_loss` myself on both arms plus two non-reserve tools.
7. Gates re-run from scratch, including a rebuilt parent baseline for all three counts.
8. Independent register sweep across the whole tree, including the round-2 subsection's own claims and the two `mcp.mdx` mirrors' changed lines compared programmatically.

**Verification-only edits, all declared and all reverted.**

| # | Edit | Result |
|---|---|---|
| P1 | Probe: 37-codepoint truth table over `is_control` / `breaks_one_line` / `check_size` | `is_control()` true for exactly the seven Cc entries and nothing else; `U+2028`/`U+2029` **pass `check_size`** and are refused only by the door; `U+2065` (unassigned Default_Ignorable) accepted by both |
| P2 | Probe: `U+2028`/`U+2029` ids × all seven tools, end to end, plus an innocent agent's block | all 14 refused, each naming `agent_id`, "single line" **and** the codepoint; no author, no lock, no trace in the block |
| P3 | Probe: 15 things that still pass the predicate, each reserved then rendered | **nothing forges a line** — `block_lines` stays 4 and the holder line stays one line in every case, including `<br>`, `\\n`, `\\u000a`, four Zs spaces, ZWJ, VS16, CGJ and a combining mark |
| P4 | Probe: eviction at `max_tokens` ∈ {1, 40, 80, 160, 4096} × {1-char, 256-char holder} | reproduces round 2 exactly: 256-char holder **evicts at 40 and 80**, survives at 160; 53 vs 308 chars at `max_tokens=1` |
| P5 | Probe: `simulate_lease_loss` driven directly, both arms + `lambo_derive` + `lambo_record_action`, beside a real §11 conflict | fenced: class only, `leaks_sql=false`, both arms. §11: holder and expiry intact |
| P6 | Probe: `U+2028` in `content`, and four unassigned Default_Ignorable ranges | `content` accepts both; the two contents **merge to one canonical key**; `U+2065`, `U+FFF0`, `U+E0080`, `U+E01F0` all pass `check_size` |
| M1 | `breaks_one_line` → `matches!(c, '\n' \| '\r' \| '\t')` (the round-1 rule) | `every_tool_refuses_an_unusable_agent_id` **FAIL**, `conflict_err_folds_every_line_forging_character` **FAIL**, and P3 flips to `accepted=true` with the forged line rendered into the block |
| M2 | `conflict_err`'s two selections → `LamboError::Conflict(msg)` | `a_lease_lost_reserve_does_not_disclose_the_operator_override` **FAIL** *and* `two_agents_through_one_server_hold_distinct_locks` **FAIL**; the `DELETE FROM session_leases …` string reproduces verbatim on both arms |
| — | `git checkout 8963b2e -- src/` for the parent baseline, then `git checkout f083a5a -- src/` | 810 / 797 / 886 |

## Round-2 findings: verification at the artifact

| # | P | Prescription | Verified how | Status |
|---|---|---|---|---|
| J1-R2-1 | P2 | Add `U+2028`/`U+2029` to the guard **and** to `conflict_err`'s fold; extend the bad-id table; correct the docstring or make it true | `breaks_one_line` (`server.rs:607`) is consulted by the guard (`:1047`) **and** the fold (`:673`) — the two cannot disagree again, which was the actual defect. **P1**: `check_size` passes both codepoints, so the door is the only thing that can refuse them; it does. **P2**: all seven tools refuse both, by the line rule, naming the codepoint. **M1** reproduces the round-2 finding exactly (accepted, holder, rendered verbatim) and turns both committed tests red. Docstring corrected and now *stronger* than the prescription: it states the rule as `Cc ∪ Zl ∪ Zp`, names the two rejected alternatives, and explains why the two separators are literals rather than a property test | **CLOSED, better than prescribed** — the review asked for two literals in two places; the remediation removed the possibility of the two places diverging |
| J1-R2-2 | P2 | Narrow the exception to its producer, not its variant; prefer the structural fix over a prefix test; make `simulate_lease_loss` reachable; pin a test; drop or condition the "wait for the expiry" advice | Structural fix taken, not the prefix test the review offered as a fallback. `LamboError::SoftLock` constructed at **exactly** `graph/reserve.rs:120` and `:154`, nowhere else in the crate. `Conflict` constructors: `memory.rs:629` (build-time lease) and `:2170` (`lease_lost_error`) — both flatten. **P5**: fenced reserve *and* release both return `conflict (the detail was logged server-side)`; no `DELETE FROM`, no `session_leases`, no lease state, and no "wait for the expiry" — so the advice question is closed by the same move rather than papered over. `simulate_lease_loss` is `pub(crate)`, still `#[cfg(all(test, …))]`, so it does not exist in a shipped binary. **M2** goes red on *two* tests | **CLOSED, and the default is now closed** — a future `Conflict` producer under `reserve_as` flattens without anyone reading the docstring |
| — | ripple | `Display` stays `"conflict: {0}"`; `err_class` maps both to `"conflict"`; ledger `error_kind` unmoved | `types/mod.rs`: both variants carry `#[error("conflict: {0}")]` — byte-identical. `err_class` has both arms → `"conflict"` (`server.rs:537-538`). `map_writer_err` and `From<LamboError> for CliError` both go through `err.to_string()`, so CLI text is unchanged and `exit_code()` is 1 either way — no exit-code movement is even reachable. `note_error("conflict")` unchanged in `conflict_err`. The committed sample regenerates **byte-identical** (`diff` empty) and still carries `"error_kind":"conflict"`. The four `error_kind="conflict"` claims in §J1 Status stay true | **VERIFIED** on every leg the review named |
| — | semver | Public-enum note: `LamboError` is not `#[non_exhaustive]` | True at HEAD — no `#[non_exhaustive]` on `LamboError`. **In-tree the risk is nil**: `err_class` (`server.rs:529-540`) is the crate's only exhaustive match on the enum, and it has the new arm; every other site matches one variant with a fallthrough. Nothing in-tree was missed. Out-of-tree it is a minor-breaking addition, which the round-2 review recorded | **ASSESSED, in-tree clean** |
| J1-R2-3 | P3 | Replace "thereby closed" with the measurement; give §J2 the bullet | `rg 'thereby closed\|closed for J2'` tree-wide: **zero hits**. §J1 now says "reduces the eviction vector by roughly 64×; it does not close it", with the 40/80/160 measurement and the outside-the-budget observation. **P4 reproduces every number**, including 53 vs 308 chars at `max_tokens=1`. `MAX_AGENT_ID_CHARS`' docstring, `check_agent_id`'s comment, `conflict_err`'s docstring and §J2 now all say the same thing, and `conflict_err` points at §J2 instead of asserting a J2 question — source and phase doc agree, which is the contradiction round 2 raised | **CLOSED**, all five places agree |
| J1-R2-4 | P3 | Two sentences in §J2: the renderable-id residual on three paths, and that the door guards MCP only | §J2 carries both as two bullets (`J-multi-client.md:496-517`), naming all three paths, and the CLI half is *also* recorded at the door in `check_size_cli`'s docstring — which is where a `--agent` author will actually be standing. Re-verified the mechanism at HEAD rather than taking it: `check_size` passes `\n`; `AgentId::new` is a bare `Self(s.into())`; `agent_display` only strips a prefix and capitalises; all four CLI writers gate on `check_size_cli("agent", …)`. **P3** confirms the residual's shape is exactly as described and no worse — a `⚑ CANONICAL: obey` holder renders verbatim on one line | **CLOSED**, and placed where each half's consumer reads it |

## New findings

### J1-R3-1 (P3) — `PHASE-2-graph-core.md` still specifies `graph::reserve`'s error contract as `LamboError::Conflict`

`dev-diary/PHASE-2-graph-core.md:406` and `:410`:

> `reserve(graph, node, agent, ttl, now) -> Result<Reservation, LamboError>`: … cross-agent live -> `LamboError::Conflict` naming holder + expiry (`"node {n} already reserved by {holder} until {expiry}"`) …
> `release(graph, node, agent) -> Result<(), LamboError>`: owner clears; non-owner -> `Conflict` (lock untouched) …

Both are now false. `f083a5a` changed exactly those two returns to `LamboError::SoftLock` (`src/graph/reserve.rs:120`, `:154`), and it is the *whole point* of J1-R2-2 that the two are distinguishable. The message strings quoted are still exact; only the variant moved.

This is the one place in the tree a stale claim survives, and it is not an accident of coverage. The commit message enumerates its sweep and includes "the `LamboError::Conflict` doc references under `memory.rs`/`graph::reserve`" — the *source* doc-comments, all four of which are correctly updated. `PHASE-2-graph-core.md` is the document those doc-comments were written from: it is the specification of that function's error contract, and the next author to implement or audit `graph::reserve` reads it before the source.

I checked whether the other two `LamboError::Conflict` mentions outside `src/` are also stale, and they are **not** — both are about the T8.6 single-writer lease, which is still `Conflict`:

* `PHASE-8-surface.md:1233` — "`Memory::build` … fails closed with `LamboError::Conflict` naming the current holder, its age, and the operator override" → `memory.rs:629`. True.
* `PHASE-8-surface.md:1382` — "`LamboError::Conflict` is printed as-is (names holder, age, `OPERATOR_OVERRIDE`) and exits 1" → true of the lease case it describes. Incomplete rather than false: the CLI's `reserve`/`release` §11 refusals are now `SoftLock` and are also printed as-is, with identical `Display` and the same exit code, so no reader is misled.

Everything else is clean. `lambo-hackathon-spec-v0.1.md`, `PHASE-1-contracts.md`, `PHASE-4-daemon.md` and `PHASE-5-recall.md` mention only `DaemonEvent::Conflict`, an unrelated enum. No document lists `LamboError`'s variants, so nothing else needs the new one.

**Remediation.** Two edits, no behaviour:

```
PHASE-2-graph-core.md:406   cross-agent live -> `LamboError::SoftLock` naming holder + expiry
PHASE-2-graph-core.md:410   non-owner -> `SoftLock` (lock untouched); no reservation -> `NotFound`
```

with a clause on the first saying the variant is `SoftLock` rather than `Conflict` because `mcp::server` renders this one intact and flattens the other (J1-R2-2). Optionally add `SoftLock` to the enum's own line at `PHASE-1-contracts.md:43`, which currently names the type only.

### J1-R3-2 (P3) — the remediation reintroduced the exact defect J1-R1-5 was raised about: a new over-long doc-comment line

`src/mcp/server.rs:784` is **109 characters** (111 bytes):

```rust
/// [`LamboServer::check_agent_id`]. Applies only at this door — `--agent` and `AgentId` itself stay uncapped
```

The parent's line was 76 characters. The remediation prepended "bounds — but does not eliminate — that eviction: see the measurement in" to the paragraph and did not rewrap, so the reference and the next sentence collided onto one line. `rustfmt` does not reflow doc comments, so `cargo fmt --check` is clean — which is exactly the situation round 1's J1-R1-5 existed to catch, and round 2 verified closed by measuring in chars with Python rather than trusting `fmt`.

The same artefact appears in the neighbouring block comment, harmlessly but visibly: `check_agent_id:1043` has a ragged 30-character `// The divergence from` line stranded above a full-width one, where inserted text pushed the old wrap point.

I confirmed the other three over-100 lines in `server.rs` are **not** this commit's: `:4338` and `:4344` are round 2's pre-existing `json!` test bodies at `:4127`/`:4133`, shifted by exactly the +211 lines this commit inserts. `src/cli/caps.rs` and `src/types/mod.rs` have **zero** lines over 100; `graph/reserve.rs:47` and the four in `memory.rs` are all pre-existing and absent from the diff. `J-multi-client.md` has **zero** lines over 96 — round 2's measurement still holds across +156 lines of new prose, which is the harder half and was done right.

**Remediation.** Rewrap `server.rs:783-785` to the file's 100-char width, e.g.

```rust
/// from another agent's context. 256 is generous for any real client id, and
/// bounds — but does not eliminate — that eviction: see the measurement in
/// [`LamboServer::check_agent_id`]. Applies only at this door — `--agent` and
/// `AgentId` itself stay uncapped (trusted, process-side).
```

and reflow the four lines around `:1043` so `// The divergence from` joins its sentence.

### J1-R3-3 (P3, advisory — `caps.rs`, not J1) — the recorded `INVISIBLE_RANGES` decision reaches the right conclusion by the wrong argument, and names the wrong revisit trigger

Round 2 asked that the `INVISIBLE_RANGES` question "be a decision rather than an oversight". It now is, at `is_disallowed_format` (`src/cli/caps.rs:160-171`) — which is the right home, and I am **not** asking for the table to change. Two things about the recorded reasoning are imprecise, and one of them will misfire.

The docstring opens by granting the criterion:

> They are invisible structural characters, which is this table's stated criterion, and they are caught by nothing else …

then declines on a different axis:

> For `content` nothing is gained by adding them: `content` already permits `\n`, so a forced line break buys an author nothing it could not already have.

That answers the *break* half. The table's criterion, in its own words at `caps.rs:144-149`, is the *invisibility* half — "renders as nothing, so a recall context block containing one looks innocuous to a human reviewer while reordering or hiding what the model actually reads". `U+2028` in `content` is not equivalent to `\n` on that axis: in a terminal review `\n` is a visible break and `U+2028` is nothing at all, and in `serve_web`'s page the asymmetry inverts. So the stated ground does not reach the stated criterion.

Consequently the revisit trigger is wrong:

> If `content` ever stops permitting `\n`, revisit this together with that predicate rather than in isolation.

`content` ceasing to permit `\n` is not the condition under which this gap starts to matter. The condition is a renderer that treats `U+2028` differently from `\n` — and two already exist in-tree, which is the same fact the `breaks_one_line` docstring 400 lines away uses as its *justification*.

**In the remediation's favour, the conclusion is right, and for a stronger reason than the one recorded.** I went looking for the forking attack and it does not exist: `normalize_tokens` splits on `char::is_whitespace()` (`graph/canonical.rs:242`), whose Unicode `White_Space` property **includes** `U+2028`/`U+2029`, so a `U+2028` in `content` cannot fork a canonical key. **P6** confirms it — two concepts differing only in `\n` versus `U+2028` merged to one. That is the argument the docstring should be making, and it is the argument that survives a renderer change.

While attacking the predicate I also found that the same table jumps every **unassigned** Default_Ignorable codepoint, not just `U+2028`/`U+2029`. All four probed pass `check_size` on `content`:

```
R3P6 default-ignorable U+2065: check_size accepted
R3P6 default-ignorable U+FFF0: check_size accepted
R3P6 default-ignorable U+E0080: check_size accepted
R3P6 default-ignorable U+E01F0: check_size accepted
```

`U+2065` sits in the gap between the table's `2060`–`2064` and `2066`–`206F` rows; `U+FFF0`–`U+FFF8` sit below its `FFF9`–`FFFB` row; `U+E0080`–`U+E00FF` and `U+E01F0`–`U+E0FFF` sit between its two supplementary rows. All are `Other_Default_Ignorable_Code_Point`, so a conforming renderer draws nothing — the table's stated criterion, exactly. None of them forges a line, so none is `breaks_one_line`'s business and none is J1's.

**Remediation** (whoever owns `caps.rs`; explicitly not J1, and not blocking): replace the "nothing is gained" clause with the canonical-key argument and the honest asymmetry — `U+2028` is invisible where `\n` is visible, but it cannot fork a key and `content` is not under a single-line contract — and change the revisit trigger from "`content` stops permitting `\n`" to "a renderer starts distinguishing them, or `content` acquires a single-line contract". Separately, decide the unassigned-Default_Ignorable ranges the same way: either widen the rows to `2060`–`206F`, `FFF0`–`FFFB`, `E0000`–`E0FFF` (minus the `E0100`–`E01EF` exception), or record why unassigned-but-ignorable is deliberately out.

## Attacks that did not land

* **Something else that passes `breaks_one_line` and forges structure.** The whole point of P3. Fifteen candidates — four `Zs` spaces (`U+00A0`, `U+2007`, `U+205F`, `U+3000`), the three `TEXT_REQUIRED_INVISIBLE` survivors (`U+200D`, `U+FE0F`, `U+034F`), a combining mark, `<br>`, a literal `\n`, a JSON-escaped `\u000a`, `[LF]`, and the marker vocabulary — every one either accepted-and-inert or refused. `block_lines` is 4 in every accepted case and every holder line is one line. Nothing forges a line, a column or a block boundary.
* **HTML markup as the successor to `U+2028`.** The natural follow-on: the docstring justifies the fix by pointing at `serve_web`, so does `<br>` forge a break where `U+2028` did? No. `web/app.js` assigns the context through **`textContent`** everywhere (`:830` for the recall block, `:89` for the generic setter) — markup is inert by construction, not by escaping. The `U+2028` fix was necessary precisely because it is the one line break that survives `textContent`.
* **Bidi and zero-width identity spoofing.** `U+202E` RLO, the whole `202A`–`202E` family, `U+200B`, `U+200E/F`, `U+2060`–`U+2064`, `U+2066`, `U+00AD`, `U+FEFF`, `U+3164`, `U+180E`, `U+061C`, `U+2800` and the `E0000` tag block are all refused by `check_size` at their own source, naming the codepoint — verified individually, so no handled case is re-reported here as new.
* **A `Conflict` producer that still reaches `conflict_err`.** Only two exist (`memory.rs:629` build-time, `:2170` lease-lost) and both flatten; P5 drove the second directly on both arms and got the class. A third producer added tomorrow flattens by default.
* **A downstream consumer that matched `Conflict` and now misses `SoftLock`.** Swept every match arm on the enum. `err_class` has both. `map_writer_err` and `From<LamboError> for CliError` both stringify, and `Display` is byte-identical, so CLI text and `exit_code()` (1 either way) cannot move. `cli/reserve.rs:86` matches only `Store(NotFound)`. The daemon's `Conflict` is `Condition::Conflict`/`DaemonEvent::Conflict`, a different enum entirely. `serve_web` never matches the type. No retry or classify path keys on the variant.
* **An in-tree exhaustive match missed by the semver note.** `err_class` is the only one, and it compiles — which is itself the proof.
* **A test that survives its own subject.** Both mutations caught, and M2 caught by *two* tests, one of them the round-1 test asserting the exception still works. The pair pins the exception from both sides.
* **The `pub(crate)` test hook leaking into a build.** `simulate_lease_loss` keeps `#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]`; clippy is clean on `ship,fixtures` and on `store-sqlite,fixtures`, which is the feature-matrix shape the original narrow gate existed for.
* **Mirror drift.** The five changed/adjacent lines of `docs/reference/mcp.mdx` and `site/src/content/docs/mcp.mdx` — the `agent_id` rule, the single-line row, the 256 row, the holder row and the new lost-lease row — extracted and compared programmatically: **byte-identical**. The files as wholes are not, and never were.
* **A stale register elsewhere.** `rg` for "thereby closed", "closed for J2" and "one variant on one path": **zero hits** tree-wide outside the review files. `AGENTS.md`, `skills/lambo-cloudops/SKILL.md`, `docs/reference/cli.mdx` and `evidence/mcp-client-stdio/README.md` carry no now-false claim. The one stale claim that does exist is J1-R3-1.
* **The round-2 subsection's own claims.** Spot-checked each: nine bad ids (counted: 9), the `BK/CR/LF/NL ⊂ Cc ∪ Zl ∪ Zp` subset claim and its "drops `\t`" rider (both true — `\t` is line-break class `BA`), `Zl`/`Zp` singleton membership, `is_control()` = `Cc`, the untrimmed-id rationale (`caller_agent` really does not trim; the `trim()` is only in the empty check), and every measured number. Nothing overclaimed.
* **The `\r` provenance claim.** `check_agent_id`'s comment says `\r` and the other C0/C1 controls are already refused upstream while the class covers them anyway. P1 confirms the exact shape: `check_size` refuses `\r`, `U+000B`, `U+000C`, `U+0085`, `U+001B`, and passes `\n` and `\t` — so the door is the only refuser of precisely the two `check_size` exempts, plus the two separators. The comment is right about which characters it is and is not the last line of defence for.

## Positive observations

* **Both blockers were fixed at the mechanism, not at the instance, and the commit message says so and means it.** The review offered a prefix test as an acceptable fallback for J1-R2-2 and a two-literal patch for J1-R2-1; the remediation declined both in favour of a typed producer and a shared predicate, and each choice removes the *class* of defect rather than the case. The three rejected alternatives for J1-R2-2 are each rejected for a checkable reason, and the sharpest of them is the one a reviewer would have pushed back with: tagging the lease loss would have fixed this instance and left the default open.
* **The fix inverts the default.** This is the property worth naming. Before, an exception was safe only while every producer of a variant happened to be safe, and the docstring carried that obligation. Now a new `Conflict` producer anywhere under `reserve_as` flattens with nobody reading anything. That is the difference between a fix and a patch.
* **M2 goes red on two tests, in opposite directions.** The disclosure test catches the leak; `two_agents_through_one_server_hold_distinct_locks` catches the over-flatten. A future author cannot re-widen or over-narrow the exception silently. Very few remediations in this repo are pinned on both sides.
* **The predicate's docstring is the best piece of prose in the change.** It states the rule as a category, names what would make it wrong (`is_control()` ≠ `Zl`/`Zp`), names the two alternatives it rejected *and why each fails*, and — the part reviewers almost never get — records what the honest spelling would be if the dependency existed. I checked all three of its factual claims and all three hold.
* **The test-hook widening is argued, not just done.** `pub(crate)` on a `#[cfg(test)]` method is the kind of change that usually arrives unexplained. Here the docstring names the alternative (a real store-level takeover from `mcp::server::tests`), what it would cost, and why the assertion is not about that — and it observes that the absence of any MCP-level test on that arm is *how* the round-1 defect survived. That is the correct root cause.
* **The residual placements went where the consumer stands.** J1-R2-4's two halves are recorded in two different places on purpose: the render-side half under §J2, the `--agent` half at `check_size_cli`'s own door. Round 2's complaint was precisely that a real risk lived only in a docstring nobody would open; splitting it by reader rather than filing both in one section is a better answer than the one prescribed.
* **The gate numbers are exact and the delta is fully accounted.** Every figure reproduces, and I rebuilt the parent for all three counts rather than trusting them: 810→812, lib 797→799, sqlite 886→888. The diff removes no test, renames none, and `#[ignore]`s none — the only deleted `fn` line in the whole change is `simulate_lease_loss`'s own signature.
* **156 lines of new phase-doc prose and not one line over 96 characters.** The width discipline round 1 asked for held on the file where it was hard, which is why J1-R3-2 is worth only a sentence.

## Gate results

Re-run from scratch in this worktree, `CARGO_TARGET_DIR=/Users/narayan/Documents/work/lambo/target`.

| Gate | Claimed | Measured | |
|---|---|---|---|
| `cargo fmt --check` | clean | clean | ✅ |
| `cargo clippy --all-targets` | clean | 0 warnings, 0 errors | ✅ |
| `… --features store-sqlite,fixtures` | clean | 0 / 0 | ✅ |
| `… --features ship,fixtures` | clean | 0 / 0 | ✅ |
| `cargo test --all --features fixtures` | 812 / 0 / 3 (lib 799/0/1) | **812 passed / 0 failed / 3 ignored**; lib **799 / 0 / 1** | ✅ |
| `cargo test --features store-sqlite,fixtures` | 888 / 0 / 3 | **888 passed / 0 failed / 3 ignored** | ✅ |
| parent `8963b2e` baseline | 810 / 797 / 886 | **810 / 0 / 3**, lib **797 / 0 / 1**, sqlite **886 / 0 / 3** | ✅ |
| delta | +2 on each | **+2 / +2 / +2**, accounted for exactly by `conflict_err_folds_every_line_forging_character` and `a_lease_lost_reserve_does_not_disclose_the_operator_override`. No test deleted, renamed or `#[ignore]`d | ✅ |
| `scripts/observability/verify.sh` | 40 ok / ALL CHECKS PASSED | **40 `ok`**, `ALL CHECKS PASSED` | ✅ |
| `make_sample.py` vs committed sample | — | `diff` empty; `"error_kind":"conflict"` still present | ✅ |
| mirror changed lines byte-identical | — | 5/5 identical | ✅ |
| tree state | — | `git status --porcelain` clean but for this file; `server.rs` md5 `1f55f6b2…`, `caps.rs` `8afc3713…`, `reserve.rs` `8ee494bd…` — all pre-edit | ✅ |

## Verdict

**CLEAN** — advisory findings **J1-R3-1**, **J1-R3-2**, **J1-R3-3**, all P3, none blocking.

Both round-2 blockers are closed, and closed at the mechanism. `breaks_one_line` does not merely add two codepoints — it removes the possibility that the guard and the fold disagree, which is what the defect actually was; and mutating it back reproduces the round-2 finding end to end, forged `⚑ CANONICAL` line and all. `LamboError::SoftLock` does not merely stop this leak — it inverts the default so the next `Conflict` producer under `reserve_as` flattens without anyone remembering a docstring; the "produced by `graph::reserve` and nowhere else" claim is a verifiable fact about the crate, not a statement of intent; and the split is pinned from both sides, so neither re-widening nor over-flattening can land quietly. The two P3s are closed with a measurement I reproduced number for number, and the residuals are placed where each half's reader stands rather than filed together for tidiness. Every gate figure reproduces, parent baseline included, and every one of the six test-count deltas is +2 with both new tests named.

The three advisories are doc precision and one paragraph's width. **J1-R3-1** is the only one worth handing forward with any emphasis, and it repeats a pattern this workstream has now hit three times: the sweep covered the source doc-comments that describe `graph::reserve`'s error contract, and missed the phase doc that *specified* it. Round 2's own note about "a claim-family sweep beats a read of the neighbourhood" applies verbatim — `rg -n 'LamboError::Conflict'` tree-wide is one query and finds it. **J1-R3-2** is a 109-character line that `cargo fmt` structurally cannot catch, in the same class as round 1's J1-R1-5. **J1-R3-3** is a `caps.rs` question the review deliberately kept out of J1's scope; the recorded decision reaches the right answer, and my objection is to the argument and the revisit trigger, not the outcome — I found the argument it should be making (`normalize_tokens` splits on `White_Space`, so no canonical key can fork) while trying to break the outcome and failing.

**J1 is ready to integrate.** The design decision under all of this is still untouched — three rounds now, and no round has needed to reopen it.

Per the **I-round-3 precedent** (CLEAN, three P3s, carried as J0 by orchestrator decision rather than remediated in a fourth round), these three qualify on the same conditions: **doc-precision only, no behaviour, no test, no gate**. J1-R3-1 is two words in a phase doc, J1-R3-2 is a rewrap, J1-R3-3 is a `caps.rs` docstring that belongs to a different owner. A fourth round would cost a full gate cycle to change no executable byte. Carry all three; J1-R3-1 should ride with whichever workstream next touches `graph::reserve` or `PHASE-2-graph-core.md`, and J1-R3-3 with whoever next opens `caps.rs`.
