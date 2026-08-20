# J — Multi-client survivability: a second agent never silently loses memory

**Goal:** on a machine running more than one agent client, every client gets working
memory: full read, full write, and a usable lock. Not a refusal, not a degraded mode,
not silence.

**Why now (found by dogfooding, 2026-08-19):** two independent clients on one machine,
Claude Code and pi v0.84.1, each spawned their own `lambo serve` per the documented stdio
wiring. The lease admitted one; the others exited 1. In Claude Code's case no error reached
the agent at all.

The lease is not the problem and must stay. It is sized to serve's shutdown budget, not to
a write, because it protects an in-RAM write-behind log rather than a row. What is wrong is
that the default wiring gives every client its own serve, turning a correct process-level
lock into an agent-level outage. **Agents never clash; serve processes do.**

---

## Order

J4 was originally last. It is first: a proxy cannot forward a call without carrying the
caller's identity, and `lambo_reserve` is already broken without it. Renumbered.

| # | Task | Depends on |
| --- | --- | --- |
| J0 | Carryover from workstream I, round 3 (CLEAN) — **DONE `0c81419`** | nothing |
| J1 | Per-call agent identity — **DONE** | nothing |
| J2 | A losing serve proxies instead of exiting | J1 |
| J3 | Writes acknowledged before the embedder — **DONE (`wt/j3`, awaiting review)** | J1 (receipt scoping) |
| J4 | Lease conflicts leave an artifact | I1 |
| J5 | Transport defaults and config layering | nothing |

J2 and J3 were previously one item. Splitting them ships the outage fix without waiting on
the write-path change.

---

## J0 — Carryover from workstream I (round 3, CLEAN — decision 2026-08-20)

**Status: done, `0c81419`.** Reviewed CLEAN with six P3 advisories at `77f119f`
([adve-review-mooshik-J0-round1.md](../adversarial-review/adve-review-mooshik-J0-round1.md)),
all six remediated in the round-1 remediation commit that follows it rather than carried
again — the three numbered items below stay as the spec they were, not as open work. The
closure narrative lives in [I-observability.md](I-observability.md)'s Handoff Log, which is
the right home for I's remediation history; this is the pointer from J's board.

That remediation was itself reviewed CLEAN — one P2, five P3
([adve-review-mooshik-J0-round2.md](../adversarial-review/adve-review-mooshik-J0-round2.md))
— and all six are closed in the round-2 remediation commit. The P2 was a gate hole, not a
prose defect: the new `verify.sh` queue-depth fixture wrote its heartbeats in stamp order,
so deleting `queued_lines()`'s `ts` sort outright still passed. Still doc-precision plus
that one fixture reorder; still no behaviour change.

Remediating the six rather than carrying them is a deliberate reversal of the decision that
created J0. The reasoning does not transfer twice: carrying I's advisories was defensible
because J1/J2/J4 rebuild the surface they sit on, so a fourth round would have re-touched
prose about to be rewritten anyway. These six are two-word register fixes, one `jq` field,
one grammatical mood, and one `verify.sh` fixture — cheap, entirely inside J0's own scope,
and none of them on ground J1–J5 rebuild. Carrying advisories twice in a row would also
stop being an exception and start being how this workstream handles P3s.

Workstream I closed CLEAN with three P3 advisories
([adve-review-mooshik-I-round3.md](../adversarial-review/adve-review-mooshik-I-round3.md)).
By decision, they ride here instead of a fourth remediation round: I's residual surface is
serve startup and ledger accounting, which J1/J2/J4 rebuild anyway. All three are
doc-precision; none touches behaviour.

1. **I-R3-1** — `src/ledger.rs:250-254`: `Ledger::open`'s docstring still asserts serve
   arms the SIGTERM handler *after* calling it; I-R2-1 inverted that. Reword to the
   current ordering (a blocking `open` there now wedges a server that never serves —
   availability, not durability). Keep the probe-placement conclusion; it stands on its
   own.
2. **I-R3-2** — `scripts/observability/README.md:320-327`: the parked-writer reading
   names the wrong transport. A parked writer writes nothing, heartbeats included, so the
   case is visible only through **live `lambo_stats`**, never in the file; the
   heartbeat-trend reading belongs to the writer-*behind* case. One clause. Optionally:
   `header()` mentions a non-zero last-heartbeat `queued` beside the dropped line.
3. **I-R3-3** — `I-observability.md`'s Handoff Log lacks an entry for `1f86792`'s two
   behavioural changes: the arming move (option 2 deferred, and why) and the
   `ledger_queued_lines` key — fold in the arithmetic deviation's reasoning
   (`accepted − written − write_failed`, because channel-full rejects never enter
   `accepted`), which is the part most likely to be re-derived.

Guidance handed forward with them: serve-startup ordering claims live in more prose sites
than any one of them signals — the cheap defence when touching that ordering is an `rg`
sweep for the claim, not a read of the neighbourhood. J2 moves this ordering again;
apply the sweep then.

Sweep once per **claim-family**, not once per change. Round 1's own conclusion was that
"sweep for the ordering claim" is too narrow: the ordering sweep came back clean, while
widening by one term — the `ledger_*` key set — found a stale count immediately (J0-R1-5),
and round 2 then found the same class again in the very file the fix touched (J0-R2-2). So
J2 gets **two** sweeps, because it moves two families: the startup ordering, **and** the
lease/endpoint schema. The second one is not hypothetical — adding `endpoint` to
`session_leases` falsifies a column list quoted verbatim at J2's own bullet below, both
`001_init.sql` files, and the two `INSERT INTO session_leases (…)` column lists in
`store/sqlite.rs` and `store/cockroach.rs`. Before finishing a change, sweep for whatever
the change is a claim *about*, and count what you find — the recurring defect across three
review rounds was never the ordering claim, it was the register not keeping up.

One more, from round 2's P2: **a fixture for an ordering claim must oppose file order and
claimed order**, or it cannot see the ordering at all. J0's `queued` fixture first wrote
its heartbeats in stamp order, so deleting the `ts` sort outright still passed the gate
40/40 — while the same commit hardened the prose calling that sort load-bearing. Write the
newer record first; prove the fixture by making the mutation it exists to catch and
watching it go red.

**Depends on:** nothing.

## J1 — Per-call agent identity

**Status: done, integrated 2026-08-20.** Three review rounds: round 1 REQUEST_CHANGES
(blocking J1-R1-1, context-block injection through a caller-asserted id), round 2
REQUEST_CHANGES (two P2 defects in the remediation itself — the U+2028/29 guard bypass and
the lease-lost `OPERATOR_OVERRIDE` disclosure), round 3 CLEAN with three P3 advisories
closed at integration rather than carried. The operator's identity ruling (cooperative,
loudly declared) plus a second ruling capping `agent_id` at 256 chars survived all three
rounds untouched. The notes at the end of this section record what shipped, what each
round changed, and the residuals handed to §J2.

The serve applies its own `--agent` to every connected client, so per-call `agent_id` is
accepted, warned about, and ignored. Under a shared writer there is no correct value for
`--agent`: any id naming one client falsifies the others.

Not a provenance nicety. **`lambo_reserve` is hard-refused for any client whose `agent_id`
differs from the serve's:**

```
lambo_reserve: refusing to take a soft lock on behalf of 'claude-orchestrator': this
process holds the session as agent 'omp-agent' ... you could release a lock you do not
hold. NOTHING WAS RESERVED OR RELEASED.
```

Fail-closed and correct, but a shared writer therefore leaves every client but one without
the only mutual-exclusion primitive lambo has. Connectivity restored, coordination still
broken.

**The work:** accept `agent_id` as an override at the Memory level rather than only at
session attach, and record it on the interaction. Small, and it gates J2, J3, and the
ledger's usefulness. Mechanically smaller than this section implies: `graph::derive`,
`record_action`, `reserve` and `release` all already take `&AgentId` as an explicit
parameter — only `Memory` hardcodes `&self.agent` at its call sites, and the ledger
already attributes lines to the *caller's* id, not the process agent.

**The catch (found reviewing J0, decided by the operator, not the implementor):**
`agent_id` arrives on the wire **unauthenticated**. Over stdio the client owns the
process; over HTTP there is one bearer token for the server, not one per agent. Honouring
the field naively restores the exact defect `require_session_agent`'s docstring names —
*"mutual exclusion that reports success without providing exclusion is worse than no
mutual exclusion"*: two clients that both default to the same id (`"claude"`) share every
lock while each is told it holds one, and any client can release a lock it does not hold
by naming the holder. The candidate designs differ materially (bind identity to the
transport connection; require distinct declared identities at attach; accept
caller-asserted identity as cooperative-only and say so in the tool description), so
**J1 does not start until the operator has picked one** — the choice is recorded here
when made.

### J1 Status — landed

**What shipped.**

* **`Memory` grew `_as` twins**, not a changed signature: `derive_as`, `record_action_as`,
  `reserve_as`, `release_as`, and the private `begin_interaction_as`, each taking
  `&AgentId` first. The plain methods delegate (`self.derive_as(&self.agent, …)`), so every
  CLI, demo and test call site is untouched. Rejected: (a) making the existing methods take
  an agent — churn across every caller for no gain; (b) an `as_agent(&id) -> handle` clone —
  a second handle shape invites someone to give it its own `ACTIVE_SESSIONS` slot or lease,
  which is precisely the confusion J1 must not create. One handle, one lease, one registry
  slot; the per-call id names a **writer**, never a session.
* **`Memory::agent()` changed meaning, and says so.** It is now the handle's *default*
  writer id plus its process identity (lease holder, `ACTIVE_SESSIONS` key, heartbeat and
  `lambo_stats` owner field). It is no longer "the only id this handle writes as". Consumers
  asking "who wrote this" must read the interaction's `agent_id`. `ACTIVE_SESSIONS` is
  untouched by per-call ids because no new handle is created, so `SecondSessionWriter`
  detection is neither weakened nor spuriously tripped by a foreign per-call id.
* **`LamboServer::attribution` is gone**, replaced by `check_agent_id` (shape only:
  non-empty, size cap, and — since the round-1 remediation below — single-line) and
  `caller_agent` (→ `AgentId` for the write path). The mismatch
  warning was **deleted rather than reworded**: after J1 the caller's id is what gets
  recorded, so a warning saying otherwise is simply false. The id is taken **verbatim and
  untrimmed** — normalising would silently merge two callers' locks, the one failure mode
  this design exists to avoid.
* **`require_session_agent` is deleted** with its call site. The refusal's own reasoning
  ("mutual exclusion that reports success without providing exclusion is worse than no
  mutual exclusion") is honoured by *providing* the exclusion instead of refusing to try:
  `reserve_as`/`release_as` contend on the caller's id, so two clients through one serve
  hold distinct locks and a non-holder still cannot release.
* **The declaration** (the operator's compensating control) is in three places: all seven
  `agent_id` param descriptions, `lambo_reserve`'s tool description, and the server
  instructions string — caller-asserted and unverified, one stable id per agent, distinct
  ids get distinct locks, a shared id shares locks. Mirrored in `docs/reference/mcp.mdx`
  and `site/src/content/docs/mcp.mdx` ("How `agent_id` is used", the `lambo_reserve`
  argument row, the quoted instructions block).
* **Ledger:** unchanged and already correct — `ledger_agent` copies the caller's `agent_id`
  onto the line. Verified for the case that did not exist before: a *foreign-id reserve
  that now succeeds* books `op=reserve, granted=true, outcome=ok, agent_id=<caller>`, and
  the only remaining reserve refusal is a real §11 conflict (`error_kind="conflict"`).
* **Read side needed nothing.** `recall`'s reservation line comes from
  `active_reservation(graph, id, now)` unfiltered by caller and renders
  `"Reserved by <holder> until <ts>"`, so it already surfaces *other* agents' locks with
  the holder named; phase-2 traversal is agent-agnostic. Pinned by a test rather than
  assumed.

**Not done, deliberately:** `Memory::demote` has no `_as` twin — it is not on the MCP
surface, so J1 added no caller for one. Nothing about authentication: no tokens, no
connection binding, no attach handshake. The lease and its fencing token are untouched.

**Sweep (claim-family rule).** Corrected: the seven param docs, the `lambo_reserve` tool
doc, the server instructions, the two `mcp.mdx` mirrors, and
`scripts/observability/make_sample.py`'s reserve-error line — whose `error_kind` was
`"refused: foreign agent"`, a class no code can emit any more; reclassified to `"conflict"`
(the only refusal that now exists) and the committed sample regenerated so the `verify.sh`
drift gate stayed green. Left, as accurate history: `adve-review-t8.2-mcp.md` and
`-r2.md` (past-tense review findings, including the ones that *proposed* `reserve_as`),
this section's own quoted refusal text above (it is the defect J1 fixes, not a claim about
today), and `skills/lambo-cloudops/SKILL.md`'s "every MCP tool takes your `agent_id`" plus
its "do not run two writer processes against one session" — both still true, the second
because the lease is untouched. `PHASE-8-surface.md`'s T8.2 finding, which promised exactly
this change, gained a one-line closure pointer rather than an edit to its narrative.
Missed by this sweep and corrected in the round-1 remediation: `evidence/mcp-client-stdio/`,
whose README carried the deleted refusal and the deleted warning in the present tense. Its
treatment is an annotation, not a rewrite — a capture is only evidence while it stays
byte-exact, so the header now declares what J1 superseded and every transcript is untouched.

**The four kit scripts, read against a two-agent ledger** (synthetic, interleaved
`agent-a`/`agent-b` traffic with one restart mid-file). Nothing is wrong; all four were
already agent-partitioned or agent-agnostic in the right places, so nothing was changed:

* `recall_first.py` filters `calls` by `agent_id` **before** cutting work sessions, so
  interleaved traffic does not merge two agents' sessions — verified: the two agents get
  separate session rows and separate compliance figures. The process restart splits *both*
  agents' sessions, which is correct: one process, one in-RAM graph, so a restart really is
  every agent's boundary.
* `_ledger.py`'s `agents()` is a sorted distinct-id set — degenerate before J1, genuinely
  multi-valued now, correct either way. `restart_times()` reads heartbeat `uptime_secs`,
  and heartbeats are per-*process*: J1 adds agents, not processes, so there is still exactly
  one restart stream. No per-agent notion of restart is needed or implied.
* `dedup_rate.py`'s time buckets aggregate every agent, and its `per_agent` block breaks
  the same traffic down. That is the right pair: dedup is a property of the shared graph and
  cross-agent matching is the *point*, so a bucket mixing agents is the interesting number.
  Worth knowing when reading the report: a single agent's rate is the "Per agent" block,
  not the bucket table.
* `warnings.py`'s "By agent (who was warned)" attributes to the caller and becomes
  meaningful for the first time.

The heartbeat's `stats.agent` stays the process agent, and no script reads it — so nothing
in the kit infers "who does the work here" from process identity.

**Done-when.** The first box is ticked: two clients through one serve process hold distinct
locks, a foreign id takes and releases its own lock, and a non-holder cannot release —
pinned by `two_agents_through_one_server_hold_distinct_locks`. "One hub" in the box's
wording arrives in its proxy sense with J2; J1 pins the shared-process sense.

**Tests** (all in `mcp::server::tests` unless noted):
`two_agents_through_one_server_hold_distinct_locks` (contention, non-holder release refused,
foreign lock granted, holder can release);
`a_foreign_agent_ids_write_is_recorded_under_the_callers_id` (asserts on the graph's
interaction `agent_id`, for both `derive` and the `spawn_blocking` `record_action` path);
`a_foreign_agent_id_is_honoured_without_an_attribution_warning` (the warning and the
one-serve-per-agent advice are gone);
`the_memory_default_agent_path_is_unchanged` (the plain `Memory` methods still stamp the
handle's agent);
`i1_record_action_reports_edges_and_reserve_reports_grant_or_refusal` (extended: the
foreign-id grant line, and `error_kind="conflict"` for the refusal);
`warnings_reach_the_text_content_not_only_structured_content` (retargeted onto
`lambo_reserve`'s advisory warning, since the attribution warning it used to ride on is
gone, and extended to assert recall names another agent's lock holder).
Added by the round-1 remediation:
`a_multiline_agent_id_cannot_inject_lines_into_another_agents_context` (the reviewer's probe,
both render paths, plus a recall as an innocent agent) and
`every_tool_refuses_an_unusable_agent_id` (the whole refusal set × all seven tools);
`two_agents_through_one_server_hold_distinct_locks` extended to pin the holder and the
expiry in the conflict text.

**Uncosted consumer dependency (J0 round 1):** the observability kit becomes genuinely
multi-agent the moment J1 lands. `recall_first.py` groups compliance by agent,
`_ledger.py`'s `agents()` enumerates ids, and `dedup_rate.py`'s bucketing plus
`restart_times()`'s work-session boundaries were all written under the
one-agent-per-file assumption that one serve stamping one `--agent` makes degenerate
today — none of them declares it. J1's Done-when includes re-reading those four against
a two-agent ledger, not just the server side.

### J1 round-1 review remediation

Reviewed **REQUEST_CHANGES** at `00cf4c9` — one P1, one P2, six P3
([adve-review-mooshik-J1-round1.md](../adversarial-review/adve-review-mooshik-J1-round1.md)).
All eight are closed in the round-1 remediation commit; nothing is carried. The design
decision is untouched: everything below is a guard, a rendering, or a declaration.

* **J1-R1-1 (P1, the blocker) — a caller-asserted `agent_id` could inject whole lines
  into another agent's context block.** `check_size` allows `\n` and `\t` on purpose,
  because both are legitimate inside a concept's `content`; this id is not content. Since
  J1 it is rendered verbatim into the T5.3 block *another* agent reads, by two renderers
  that do not sanitise — the soft-lock holder (`recall::format::reservation_warning`) and
  the §13 conflict sentence's writer (`recall::format::conflict_warning`, which needs no
  lock at all, just one `lambo_derive`). The reviewer's probe landed a line wearing
  Lambo's own `⚑ CANONICAL` marker; it is now a committed test that fails without the
  guard. **The guard refuses a line-forging character *class* in
  `check_agent_id`** — every `Cc` control (so `\n`, `\r` and `\t`, the three round 1
  named, plus the rest) and `U+2028`/`U+2029`, the whole of `Zl`/`Zp`. Round-1
  remediation refused the three literals; that list was incomplete the day it was written,
  and round 2 found the gap (see **J1-R2-1** below), so the rule is now a predicate shared
  with `conflict_err`'s fold. The refusal names the parameter, the codepoint and the
  reason.
  **At the door, not in `AgentId::new`:** the type is also built from the operator's own
  `--agent` by the CLI and by library callers — trusted input on the same side of the
  boundary as the process — so tightening the type would change its semantics for every
  caller, which is not J1's to do. `check_agent_id` is the single place an unauthenticated
  remote string becomes a write identity and a lock name. Nothing was added to
  `recall::format`: sanitising there would put the guard downstream of the graph, where a
  poisoned id is already durable.
  **Length deliberately not tightened by the remediation, and the consequence declared
  rather than dropped:** an over-long id forges no structure, and an MCP-only cap would
  diverge from `--agent` and from `AgentId` itself — but because assembly keeps the longest
  score-ordered prefix that *fits* `max_tokens`, dropping whole blocks, a 16 KiB holder id
  can evict the very block it annotates from another agent's context.
  **Ruled the same day (operator, 2026-08-20): capped at the door.** `MAX_AGENT_ID_CHARS
  = 256` in `check_agent_id`, beside the single-line guard — an id is a name other agents
  read, 256 is generous for any real client id, and the divergence from the uncapped
  `--agent`/`AgentId` is deliberate (that door is where unauthenticated remote identity is
  policed; trusted process-side callers keep the type's semantics). Boundary pinned from
  both sides in `every_tool_refuses_an_unusable_agent_id`: 257 refused on all seven tools,
  exactly 256 accepted.
  **The cap reduces the eviction vector by roughly 64×; it does not close it** — round 2
  measured a 256-char holder still evicting the block it annotates at `max_tokens` 40 and
  80, surviving only from ~160 up, and found the reservation warning line rendered
  *outside* the token budget altogether (at `max_tokens=1` the block is 53 chars with a
  1-char holder and 308 with a 256-char one). Eviction never needed the uniform cap's
  headroom, only a holder line that is a large fraction of the budget. So the **length**
  half is closed for any realistic budget, and the **neutralise-on-render** half stays
  open — carried as a §J2 residual, not closed here (**J1-R2-3**).
* **J1-R1-2 (P2) — the loser of a race was told `conflict` and nothing else.** `tool_err`'s
  N4 policy discards a `Memory` error's message because it can interpolate a DSN, a store
  URL or a driver string. §11's two conflict messages carry none of that — a node id the
  caller just sent, the holder's id, an expiry — and the last two are *already* model-facing,
  since recall renders the same pair into the context block. They are also exactly what
  "coordinate by ids" needs. So the reserve path gets `conflict_err`: **one producer on one
  path**, not a general opening of N4. Everything else on that path still goes through
  `tool_err`, the ledger books the same `error_kind="conflict"`, and the message is folded to
  one line on the way out as defence in depth for a holder that entered by another path.
  Round-1 remediation selected that exception by matching the `LamboError::Conflict`
  *variant*, which opened it for every producer of that variant — including the lease-lost
  fence, whose message carries operator-only SQL. Round 2 caught that as a live regression
  (**J1-R2-2** below); the selection is now its own variant, `LamboError::SoftLock`,
  produced by `graph::reserve` and nowhere else.
* **J1-R1-3 (P3)** — the four `warnings` vectors that could no longer hold a warning are
  gone; `structuredContent` keeps its `warnings` key (response shape) as a literal `[]`, with
  a comment at `derive_impl` saying a future warning must also go through `attach_warnings`.
* **J1-R1-4 (P3)** — the split `use crate::types::` imports are one line.
* **J1-R1-5 (P3)** — both over-long lines rewrapped to their local width: the
  `attach_warnings` doc comment to ~78, and the `mcp.mdx` fenced instructions block to ~88
  in both mirrors, wrapped the way the rest of that block is rather than mirroring the Rust
  string's breaks. The two remaining >100-char lines in `src/mcp/server.rs` are pre-existing
  `json!` bodies inside tests, which `rustfmt` does not reformat and J1 did not touch.
* **J1-R1-6 (P3)** — `evidence/mcp-client-stdio/` annotated, not rewritten; see the sweep above.
* **J1-R1-7 (P3)** — `every_tool_refuses_an_unusable_agent_id` pins **nine** bad ids
  across all seven tools — empty, blank, `\n`, `\r\n`, `\t`, `U+2028`, `U+2029` (the last
  two added by J1-R2-1), oversize, and over-cap (257) — and asserts each refusal *names*
  `agent_id` so a downstream failure cannot pass for the guard; exactly 256 is asserted
  accepted. Written as its own test rather than folded into
  `bad_parameters_are_refused_as_readable_tool_errors`, whose table is per-tool parameters;
  this one is the parameter all seven share.
* **J1-R1-8 (P3)** — declared in `src/daemon/conflict.rs`'s NEW-4 block: J1 is what makes
  same-instant collisions non-degenerate, so `writer` ("smallest interaction id at that
  instant") is now a deterministic choice between two *live* agents. Behaviour unchanged and
  still right for its purpose — the §13 sentence's job is to make the reader look — but it
  can now name the wrong one of two real agents, which is why J3's Done-when asks for it to
  be measured.

### J1 round-2 review remediation

Reviewed **REQUEST_CHANGES** at `8963b2e` — two P2, two P3
([adve-review-mooshik-J1-round2.md](../adversarial-review/adve-review-mooshik-J1-round2.md)).
All eight round-1 findings were verified closed at the artifact, under mutation in both
directions; the two blockers are defects **in the round-1 remediation itself**. All four
are closed here; nothing is carried. The design decision is untouched for a second round.

* **J1-R2-1 (P2) — the single-line guard enforced a narrower rule than its own docstring
  claimed.** `U+2028 LINE SEPARATOR` and `U+2029 PARAGRAPH SEPARATOR` slipped every
  layer: they are `Zl`/`Zp`, and Rust's `char::is_control()` is `Cc`-only, so `check_size`
  passes them; they are absent from `INVISIBLE_RANGES`, so the invisible-character table
  passes them; and the guard looked for three literal characters. The reviewer's probe made
  both the soft-lock holder and landed the raw codepoint in another agent's T5.3 block.
  P2 rather than P1 because to a tokenizer it is still one line — but they are *forced*
  line and paragraph breaks in CSS text layout, and `cli::serve_web` serves the context
  block verbatim into a page, where the forged break becomes real while a terminal shows
  nothing.
  **Fixed as a class, not as two more literals.** One predicate, `breaks_one_line`
  (`c.is_control() || c == '\u{2028}' || c == '\u{2029}'`), used by both
  `check_agent_id`'s guard and `conflict_err`'s fold so the two cannot disagree about what
  "one line" means — they did. `is_control()` is named for the *category* `Cc`, so the rule
  stays complete if `check_size`'s `\n`/`\t` exception table is ever widened again;
  `U+2028`/`U+2029` are written out because they are the entire membership of `Zl` and `Zp`
  and this crate has no Unicode-category dependency to test the property with. Two
  neighbouring rules were considered and rejected in the comment: *any* `White_Space`
  character (too wide — an ordinary space must stay legal, since ids are untrimmed and
  `"a"` and `"a "` are deliberately two agents) and the review's line-break classes
  `BK`/`CR`/`LF`/`NL` (a strict subset of `Cc ∪ Zl ∪ Zp`, and it drops `\t`). Anything
  merely invisible stays `check_size`'s business; this predicate answers one question only.
  Pinned on both users of the class: `every_tool_refuses_an_unusable_agent_id` gained the
  two codepoints (nine bad ids × seven tools), and
  `conflict_err_folds_every_line_forging_character` calls the fold directly —
  deliberately, since the door now makes such an id unreachable
  through a tool, and the fold exists precisely for the ids that never pass the door.
  Mutation-checked: reverting the fold to the three literals turns it red.
  Whether `U+2028`/`U+2029` also belong in `INVISIBLE_RANGES` for `content` is left to
  `caps.rs`, as the review asked — `content` already permits `\n`, so nothing is gained
  there, and the decision is now recorded at `is_disallowed_format` rather than made
  silently.
* **J1-R2-2 (P2, the regression) — `conflict_err` opened N4 for a *variant*, and
  `graph::reserve` is not its only producer.** `Memory::reserve_as`/`release_as` enter
  `begin_write_sync()` **before** the graph, and a fenced handle's `lease_lost_error()` is
  a `Conflict` too — one interpolating `store::lease::OPERATOR_OVERRIDE`. So after the
  round-1 remediation `lambo_reserve` handed a model
  `… force a takeover: DELETE FROM session_leases WHERE session_id = '<session>';`, a raw
  statement against an internal table that reads as an instruction, where the parent had
  returned `conflict (the detail was logged server-side)`. `redact_urls` was never the
  missing piece: the string has no `://`.
  **Discriminated structurally, and in the direction that fails closed.** `graph::reserve`
  and `graph::release` now return a variant of their own, `LamboError::SoftLock`, and
  `conflict_err` is selected against *that*. Three options were weighed. Tagging the lease
  loss instead (a `LeaseLost` variant) would have fixed this instance and left the next
  `Conflict` producer under `reserve_as` leaking again — it keeps the default open. A typed
  §11 payload (`{node, holder, expiry}`) is the strongest containment on paper, but the two
  §11 messages differ in shape — release names the *caller*, not an expiry — so it needs
  optional fields or two variants and touches far more code for no extra guarantee.
  Matching at the `reserve_impl` site is not available: `reserve_as` returns one `Result`,
  so the two sources are not distinguishable there without changing `Memory` anyway. The
  chosen split inverts the default — every `Conflict`, present or future, flattens through
  `tool_err` without anyone having to remember the docstring — and costs nothing
  observable: `Display` is byte-identical (`"conflict: {0}"`), so the CLI's text and its
  subprocess matchers stand, and `err_class` maps both variants to `"conflict"`, so the
  ledger's `error_kind` does not move. The "wait for the expiry or work elsewhere" advice
  the review flagged is true again for the same reason: a §11 soft lock expires, the fenced
  handle it used to reach never does.
  **The test gap the review named is closed too.** `Memory::simulate_lease_loss` is now
  `pub(crate)` (still `#[cfg(test)]`, so it does not exist in a shipped binary), which is
  what let `a_lease_lost_reserve_does_not_disclose_the_operator_override` exist at all: it
  latches the fence and asserts both arms of `lambo_reserve` flatten to the class, with
  `DELETE FROM`, `session_leases`, the lease state and the soft-lock-only advice all
  absent. Widening a test hook by one crate is the cheaper half of the trade — the
  alternative, driving a real store-level takeover from `mcp::server::tests` as
  `memory.rs`'s own fence test does, needs a second `Memory` on a shared store and a lease
  acquisition, none of which the assertion is about. Before this commit the reserve path's
  lease-lost arm had **no** MCP-level test, which is how a variant match came to render
  operator SQL without anything going red.
* **J1-R2-3 (P3) — the cap ruling told J2 the rendering-side question was closed.** It is
  not: measured, a 256-char holder still evicts the block it annotates below ~160
  `max_tokens`, so the vector is reduced ~64×, not closed, and the reservation line renders
  outside the budget entirely. The ruling paragraph above now says that, and the residual
  is carried under §J2 where its consumer reads it. `conflict_err`'s docstring, which still
  called it a J2 question, now points at §J2 too, so source and phase doc agree.
* **J1-R2-4 (P3) — the residual that *is* real lived only in a docstring J2 has no reason
  to open.** The reasoning stands and no code change was asked for: a cooperative-identity
  design cannot also promise a self-chosen name is inert, and sanitising in
  `recall::format` would sit downstream of the graph where a poisoned id is already
  durable. What moved is where it is written down — two §J2 bullets below, plus the second
  half the review found: `check_size_cli` passes `\n`, so `--agent $'x\ninjected'` writes a
  **durable** multi-line interaction author that `conflict_warning` renders unsanitised.
  Trusted local operator poisoning their own graph, so P3 — but it outlives the process,
  which the RAM-only reservations do not. Documented at the CLI door (`check_size_cli`'s
  docstring, naming why the MCP guard is deliberately not mirrored there) and carried in
  §J2; no guard added, per the review.

## J2 — A losing serve proxies instead of exiting

On refusal, become a thin proxy to the holder and forward every tool call.

* Add an `endpoint` column to `session_leases` (before J2: `session_id`, `holder`,
  `acquired_at`, `expires_at`, `current_token` — **six columns since**, and that
  five-column list is the one this section's own sweep instruction predicted
  would go stale).
* `serve` always binds a local endpoint, a unix socket keyed by session, even under
  `--transport stdio`. Being reachable stops being a transport choice.
* A refused serve reads the endpoint from the lease it lost, connects, proxies. First
  process to start becomes the hub.

Full read and write for every client, real read-your-writes, no staleness label, and **no
client config change at all**: the stdio wiring that broke would simply work. A proxy is
also cheaper than a serve, running no `store::load` replay and holding no in-RAM graph, so
N clients cost one graph instead of N.

Costs are operational rather than semantic: a socket to bind and clean up, a stale endpoint
after unclean holder death (bounded by the 45s TTL), a busier startup path.

**Two catches found before J2 was written (J0 round 1, verified at the source):**

* **The proxy branch reopens I-R2-1's hole through a new door.** A *refused* serve never
  reaches `build_memory`'s success path, so everything the proxy does — bind, read the
  lost lease's endpoint, connect — runs **above** the shutdown-arming point at
  `serve.rs:795`, unguarded, holding a socket. And the existing regression test cannot
  see it: `serve_pre_handshake_durability`'s deliberately loose matcher fires on
  "session attached", which a proxy never logs. J2 must place the arming relative to the
  refusal branch *first*, as a design decision rather than a consequence, and extend that
  test with a proxy-path case whose matcher fires on whatever line the proxy does emit.
* **Unconditional binding collides with `authorize_bind`.** `authorize_bind` runs before
  `build_memory` *specifically* so a misconfigured bind costs nothing and leaves no lease
  behind — its docstring at `serve.rs:754-759` states the reason out loud, and "refusing
  here means no lease is taken" stops being true the moment every serve binds a socket.
  That sentence and the ordering it justifies must be restated deliberately, not
  falsified in passing.

**Two rendering-side residuals handed over by J1 (round 2, J1-R2-3 / J1-R2-4):**

* **A single-line, instruction-shaped `agent_id` is still rendered verbatim into other
  agents' context, on three paths** — the soft-lock holder
  (`recall::format::reservation_warning`), the §13 conflict sentence's writer
  (`recall::format::conflict_warning` → `agent_display`, which needs no lock at all, just
  one `lambo_derive`), and `conflict_err`'s refusal to the loser of a race. J1 refused the
  *unrenderable* id and capped its length; it deliberately did not neutralise a renderable
  one, because the cooperative-identity design declares out loud that a caller names
  itself and sanitising downstream of the graph would arrive after the id is durable. J2
  is where that comes due: this is exactly the weighing that changes when clients stop
  being local and one bearer token authenticates the server rather than each agent. The
  length half is closed for any realistic budget (256 chars), with the measured residual
  recorded in §J1 — under ~160 `max_tokens` a max-length holder still evicts the block it
  annotates, and the reservation line is rendered outside the budget.
* **The single-line guard is on the MCP door only, so a durable multi-line author can
  still enter through `--agent`.** `check_size_cli` passes `\n` (legitimate inside
  `content`), so `lambo derive --agent $'x\ninjected'` writes a genuinely multi-line
  interaction author, which **persists** and which a later `serve`'s recall renders
  unsanitised through `conflict_warning`. That is a trusted local operator poisoning their
  own graph — P3 — but reservations are RAM-only and per-process while interactions are
  durable, so it is the one J1 residual that outlives the process, and it is the shape J2's
  shared-graph model makes interesting. Recorded at the door in `cli::caps::check_size_cli`.

Per the claim-family rule above: J2 runs **two sweeps** before it lands — startup-ordering
claims, and the lease/endpoint schema (this section's own column list, both
`001_init.sql` files, and the `INSERT INTO session_leases (…)` lists in
`store/sqlite.rs` and `store/cockroach.rs` all quote the current five columns).

**Considered and rejected: a durable write-intent queue.** The loser appends intents, the
holder drains them. Rejected because **validation needs the graph** — resolving concepts
and checking resolved-reflexive pairs cannot happen in a process holding none, so a queued
write is validated late, which is the silent-failure shape being designed out. It also
answers a contention problem that does not exist. Keep in reserve only for the
holder-restart window, where nothing else helps.

**Fallback if always-binding proves unworkable:** read-only attach, exposing the read verbs
against the durable store and returning the lease conflict from write verbs as a tool
result. Strictly worse: a store reader trails the holder's in-RAM tail by up to one flush
interval, so every response would need a freshness label. The CLI already models both
halves — `recall` takes no lease, `stats` marks writer-only fields unavailable.

### J2 Status — landed

**Status: implemented on `wt/j2`, five stages, `7f51bb6` → `8e64fc8` → `275b418`
(+ this note), then remediated in four commits after the round-1 review — see
[J2 round-1 review remediation](#j2-round-1-review-remediation), which is where
every claim below that the remediation changed is corrected.** The outage that created workstream J is closed on the path it
happened on: two clients on one machine, both wired over stdio, no client
configuration change, both with full read, full write and a usable lock. The
2026-08-19 probe is a committed test.

**What shipped.**

* **`session_leases` gained a nullable `endpoint`** — both `001_init.sql` files,
  both SQL adapters with an idempotent converge (`ensure_column` on sqlite,
  `ADD COLUMN IF NOT EXISTS` on cockroach) so an already-provisioned store needs
  **no re-provision**, and `MemoryStore` for parity. `LeaseHolder` and
  `LeaseInfo` carry it; `GraphStore::read_lease` reads it. It rides on
  `LeaseHolder` rather than on `acquire_lease`'s signature, which kept ~20 call
  sites untouched, and it is deliberately **not** part of `LeaseHolder::token()`:
  the token is the identity a refresh and a release match on, and it must stay
  stable even if a holder's reachability changed under it.
* **Nullable, no default, and the absence is meaningful.** Only `serve`
  publishes an endpoint, because only a serve process is reachable. A CLI writer
  holds the lease for one verb and is not proxyable, so its row says NULL and a
  refused serve reads that as "no hub here" and waits rather than dialling
  nothing. A fabricated address would be worse than an honest absence.
* **Every holder binds a session endpoint** (`src/mcp/endpoint.rs`), under either
  transport, serving the same `LamboServer` over a unix socket — one MCP session
  per connection, all against the one `Memory`, capped by the same
  `--max-sessions` ceiling the HTTP transport uses.
* **A refused stdio serve proxies** (`src/mcp/proxy.rs`) instead of exiting 1.
* **No `Cargo.toml` change *for the transport*.** `UnixStream` is already an rmcp transport:
  the `transport-io` feature in use pulls `transport-async-rw`, whose
  `IntoTransport` covers any `AsyncRead + AsyncWrite`, and the wire is the same
  newline-delimited JSON-RPC stdio speaks. The `client` feature was not needed
  because the proxy is not an MCP client. (The stages landed with **no**
  `Cargo.toml` change at all; the round-1 remediation added one line, `libc`, for
  `geteuid` — see J2-R1-3.)

**Design decisions, and why.**

* **The proxy is a byte-level JSON-RPC line pipe, not a tool-level forwarder.**
  It copies frames without deserializing them. Four consequences decided it: the
  caller's per-call `agent_id` crosses **verbatim** by construction, so J1's
  untrimmed-id contract cannot be violated in transit (that is *why* J1 gated
  J2, and a forwarder that rebuilt arguments is exactly where that regression
  would appear); the tool surface cannot drift, because schemas, descriptions
  and protocol negotiation all come from the real holder; notifications, `ping`,
  cancellation and progress forward for free; and it runs no `store::load`, holds
  no graph and needs no embedder, so N clients cost one graph. **Rejected:** a
  backend enum on `LamboServer` (`Local(Arc<Memory>) | Remote(hub)`) with seven
  dispatch sites — larger, needs a JSON-RPC client, re-serializes arguments, and
  duplicates or diverges the schemas. It is kept in reserve because it is the
  only shape that supports in-process promotion.
* **The socket address is derived, and the store is in the hash.** A function of
  session id and store identity that creates nothing and binds nothing:
  `$XDG_RUNTIME_DIR/lambo`, else `/tmp/lambo-<uid>` — **two rungs, not three**
  (created 0700 and then checked three ways — not a symlink, owned by this euid,
  mode granting nothing to group or other — which together with the per-uid name
  is what makes the shared `/tmp` fallback safe rather than assumed safe), plus a
  cosmetic 16-char session prefix and 16 hex of FNV-1a. *(The stages shipped
  three rungs ending `.../lambo`, with a follow-the-symlink mode check and an
  "I/O-free" claim; the uid suffix, the other two checks and the corrected claim
  are J2-R1-2/J2-R1-3, and the `TMPDIR` rung's removal is J2-L1 — it varied per
  client product, which is the one thing a shared address may not do.)* The store half is load-bearing: two serves under
  one session name against *different* stores are two graphs, and a session-only
  path would have them fight over one socket and let a proxy forward into the
  wrong one — and the store's *identity*, not its spelling, is what is hashed:
  the sqlite path half is canonicalized first, because `path = "./lambo.db"`
  names a different file from every cwd and hashing it verbatim gave two graphs
  one socket (J2-R1-2). Hashing also keeps a DSN's password out of both the
  filesystem and the lease row. FNV-1a is written out rather than `DefaultHasher`, which is
  explicitly unstable across Rust releases — this hash is baked into a path two
  processes must agree on across builds, so a compiler upgrade must not move a
  session's endpoint out from under a running holder. `SUN_PATH_MAX` is checked
  at 104 (macOS/BSD) on every platform, so a path that works here works
  everywhere; the headroom arithmetic against the tightest measured base
  directory is written at the prefix constant, and widening it spends that
  headroom deliberately.
* **A store no second process can see gets no endpoint and no proxy.**
  `MemoryStore` and an in-memory SQLite database have no address to hash, so two
  such holders would derive the *same* path and the second's stale-socket
  cleanup would unlink the first's live socket — and they cannot collide across
  processes anyway, so there is nothing to proxy. A refused serve there behaves
  exactly as it did before J2.
* **`Attach`, not a new `LamboError` variant.** J1-R2-2's rule is that a code
  path must never be selected by matching an error variant with several
  producers. The cheaper way to honour it was not to widen the enum at all:
  `MemoryBuilder::build` still returns the byte-identical
  `LamboError::Conflict`, and the one caller that needs the structure calls
  `build_attach` instead. No `err_class` change, no N4 question, no CLI text or
  exit-code movement, no test churn downstream.
* **Migration is additive, not a re-provision.** Existing rows get NULL, which
  reads as what a pre-J2 holder in fact did. Pinned by
  `a_pre_j2_lease_table_gains_the_endpoint_column_on_init`, which builds the old
  five-column table by hand and runs the real `init_schema` over it. The dogfood
  rig's `lambo-dev.db` survives.

**The two J0 catches, answered rather than routed around.**

* **The `authorize_bind` collision is dissolved by construction.** The endpoint's
  *address* is derived, so it is published into the lease row by the very acquire
  that takes the lease, while the *socket* is bound only afterwards and only by
  the winner. `authorize_bind`'s sentence — "refusing here means no lease is
  taken" — is therefore still **literally true**: a loser binds nothing, creates
  nothing and unlinks nothing. That function gained a "What J2 changed" section
  restating the claim rather than letting the new behaviour quietly falsify it.
  J2 adds no new pre-lease **refusal** at all: the `sun_path` length check
  degrades to "no endpoint" rather than stopping the serve (J2-R1-5), and the
  derivation stays in the pre-lease group for the reason that group exists — it
  creates nothing and binds nothing. Two further benefits fell out: the row
  carries the endpoint from the instant the lease exists, so there is no window
  in which a refused racer reads a leased row with no endpoint; and **the lease
  is what licenses the stale-socket unlink** — while we hold it, a socket file at
  this path cannot belong to a live holder, so it is a crashed one's. Unlinking
  before winning the lease would delete a healthy hub's socket.
* **The proxy branch is deliberately not armed for durability**, and the comment
  at the branch says so. The hazard was real, but the answer was to notice that
  the *hole is not there*: what the arming protects is `Memory::close`, and a
  proxy has no lease, no write-behind tail and no graph, so nothing a handler
  could save. Arming for durability would be theatre. A registration **is**
  installed for **liveness** — it is how the pump's `select!` learns to stop —
  and it is polled first, so this cannot make the process SIGTERM-immune the way
  arming above the lease-taking attach would — which under J2 would mean arming
  above `resolve_role`, a loop allowed to run 50 seconds by design (J2-R1-7). `serve_pre_handshake_durability`
  gained the proxy case with **its own sync point**, `"proxying to the session
  holder"`, precisely because the review was right that the loose
  `"session attached"` matcher never fires for a proxy — anchoring on it would
  have produced a test that passed without signalling anything. Negative control
  run: replacing the proxy's shutdown future with `pending()` turns the new case
  red.

**The wedge invariant (operator ruling, and the most important thing here).**

**A proxy never acquires the lease.** It reads the row and nothing more.
Winning a lapsed lease mid-session is the obvious and wrong move: this process
cannot serve its own client afterwards, because that client's MCP session was
established with the dead holder — so it would sit *heartbeating* a session it
cannot answer, wedging every process on the machine for as long as it lived. That
is strictly worse than the exit-1 J2 replaces. **Acquisition and promotion are
one decision, not two**; while there is no promotion, acquisition is forbidden.
It is stated at `HubProxy::run`, where a future author would violate it, and
pinned by `a_dead_holder_leaves_the_proxy_honest_and_the_lease_unclaimed`.

The one place that *may* acquire is `resolve_role`, the startup election, and the
reason is exactly the invariant's: it runs before a single byte has been
exchanged with this process's own client, so a win there makes a real holder that
can actually serve. It waits for either a reachable holder or a lapsed lease,
logging progress — but only for as long as a client will sit through
(`ELECTION_BUDGET`, 20s), and it does arithmetic on the row's `expires_at` rather
than waiting blindly: a lease that will not lapse inside the budget is refused at
once with the seconds named. *(The stages waited one `LEASE_TTL` plus 5s = ~50s
and argued that "a ~50s startup that ends in working memory beats a fast exit 1,
but it is a startup delay and a client with a short spawn timeout would see it."
The live probe turned that caveat into a measurement — `opencode` 1.18.18 gave up
at 31.96s and the model then had **no** lambo tools — so the trade was wrong in
the direction the caveat pointed. J2-L2.)*

**One deviation from the plan, forced by evidence.** Deferring the recorded-
initialize replay was the instruction, and it turned out to be incompatible with
the requirement it accompanied ("kill the holder, start a fresh serve, the
proxy's next call succeeds with no proxy restart"). Written first without a
replay, that case **hung**: a new holder's rmcp server does not answer
`tools/call` on a connection that never sent `initialize`, because MCP session
state lived in the dead holder. So `Handshake` records the client's own
`initialize` and `notifications/initialized` frames verbatim and replays them
into each new connection, swallowing the duplicate `initialize` response through
the *same* `BufReader` the pump then owns (a fresh reader could drop bytes the
first had already buffered). The swallow is matched by **id** and bounded in both
time and frame count, which it was not at the stages (J2-R1-8, J2-R1-12). This is **reconnect, not promotion** — no lease is
taken and the wedge invariant is untouched. Residual, stated at the type: the
client keeps the *old* holder's `serverInfo` / `capabilities` /
`protocolVersion` view. Identical for two holders of one binary, which is every
case on one machine; possibly stale across lambo versions. A narrower failure
than "memory is gone until you restart the client".

**The two J1 residuals — does J2 change their exposure? Yes, and this is the
answer, not a deferral.**

Neither residual's *attacker set* widens. The endpoint socket is 0600 inside a
0700 directory, so only the same uid can reach it — and the same uid could
already open the store directly, which is strictly more power. What changes is
that **both residuals become reachable in practice for the first time.**

* Before J2, N clients on one machine produced one live graph and N−1 dead
  processes. The instruction-shaped `agent_id` rendered into *another agent's*
  T5.3 block needed two agents sharing one live graph, which the outage made
  rare. After J2 that is the **normal** configuration: agent A really does read
  the id agent B chose for itself, on all three paths (`reservation_warning`,
  `conflict_warning` → `agent_display`, and `conflict_err`'s refusal). J2 does
  not widen who can attack; it makes the residual live. **The
  neutralise-on-render half is therefore now the load-bearing one**, and its
  priority should rise accordingly. The length half stays closed for any
  realistic budget.
* The `--agent` CLI door (`check_size_cli` passes `\n`, so
  `lambo derive --agent $'x\ninjected'` writes a durable multi-line interaction
  author) is untouched by J2 — same door, same trusted-local-operator framing.
  Its **blast radius** widens the same way: a poisoned durable author used to be
  rendered into that one process's own recalls, and is now rendered into every
  client attached to the hub. Still P3, still one operator poisoning their own
  graph, but the "it outlives the process" property now meets an audience.

**Not done, deliberately.**

* **No in-process promotion**, so *"a new holder is electable within one
  `LEASE_TTL`"* holds in its literal sense — the lease lapses and the next
  `serve` **start** wins it, which `resolve_role`'s election does inside one
  process at startup — but **not** in the strong sense of an already-running
  proxy electing itself mid-session. That is an accepted residual. Whoever
  builds it inherits two things: the wedge invariant above (acquisition may only
  be unlocked *together* with promotion), and the shape that fits a byte pipe —
  the proxy already dials "the current endpoint" on every reconnect, so a
  promoted proxy binds the socket and connects to **itself**, which localises the
  work to the replay that already exists. Measured cost of that self-loop: the
  hop is 0.31–0.48 ms, under 1% of any call that embeds.
* **`--transport http` keeps working exactly as-is.** A refused http serve still
  exits 1: its client-facing wire is not line-framed, so the pipe does not
  apply, and the outage J2 exists to fix is the stdio one, where the client
  spawns the process itself and never chose a port.
  `serve_single_writer_lease.rs` moved its fail-closed assertion onto that
  transport, where a refusal is the designed outcome rather than a missing
  feature; asserting exit-1 on the stdio path would pin the defect J2 removed.
* **A proxy books no ledger lines of its own.** It opens no ledger, spawns no
  heartbeat and builds no `LamboServer` — it is a pipe. The proxy-degraded state
  therefore has no artifact beyond its stderr, which is **J4's** "a refused
  lease acquisition appears in the ledger from both sides", now with a second
  side worth recording: not only *refused* but *proxying*, and *proxying to a
  holder that stopped answering*. Handed to J4 rather than taken here, because
  a ledger in the proxy is a second `Ledger::open` on a startup path whose
  ordering J2 already moved once.
* **The pump's frame writes are still unbounded arm-body awaits — six `Self::send` sites,
  not two** (J2-R2-1, round 3; count corrected J2-R3-3). Writes to the holder and to the
  client's stdout are neither raced against shutdown nor budgeted, because a write abandoned
  mid-frame delivers a torn JSON line and this pipe may never do that (`Framed::Torn` exists
  for the receiving half of the same rule). Each is bounded by its peer draining the socket —
  a different shape from the lease-row read `DIAL_BUDGET` replaced, since a peer that never
  reads is itself already wedged, whereas a row read stuck behind a flush at the connection
  pool wedged a *healthy* proxy talking to a *healthy* holder. **One site couples this to the
  other declined bound (J2-R3-3): `answer_lost`'s write burst is as long as the `inflight`
  list, whose cap J2-R2-7 declined — so the receipt ceiling J3 must derive bounds BOTH
  residuals at once, and they should be closed together.**
  Closing it means deciding what to do with a partially-written frame to a
  live peer, which is a behaviour decision rather than a bound, and is not taken here.
* No async ack, no receipts (J3). No auth. No lease weakening. No durable
  write-intent queue.

**Sweep 1 — serve-startup ordering claims (11 sites, 3 stale at the stages; 4th
found by the round-1 review, and the family's central noun was the miss — see
J2-R1-7. The table below carries the corrected verdicts.)**

| Site | Claim | Verdict |
| --- | --- | --- |
| `mcp/serve.rs` `authorize_bind` docstring | "runs FIRST … refusing here means no lease is taken" | **restated deliberately** with a new J2 section; still true |
| `mcp/serve.rs` `serve()` arming comment | enumerates the startup work below the arming | **STALE** — the endpoint bind and its accept loop were missing; added |
| `PHASE-8-surface.md` `src/mcp/serve.rs` entry | "both transports" | **STALE** — annotated with the third listener + the new module |
| `PHASE-8-surface.md` Level B note | "`build_memory` takes `ResolvedBackends`, not a config path" | **STALE-adjacent** — still true; annotated with the added parameter and why one-resolve is unchanged |
| `ledger.rs:253` | "`shutdown_signal()` is the first statement once `build_memory` returns; this call is the next one" | **STALE — sweep 1 missed this (J2-R1-7).** The second clause was checked and passed ("the bind sits below `LamboServer`, so `Ledger::open` is still next", which is true); the *first* clause went stale, because `serve()` does not call `build_memory` at all any more. Rewritten to name `resolve_role` |
| the `build_memory` noun itself, tree-wide | nine sites describe serve startup in its terms | **STALE — the family's central noun (J2-R1-7).** `rg build_memory` finds zero call sites; it survives as a `pub` library entry point re-exported at `mcp::`. All nine rewritten to name `resolve_role`, and `build_memory` docstringed as the library-only entry point it now is. One of the nine (`serve.rs`'s arming comment) carried a materially **better** argument left unwritten: the thing arming-above would make SIGTERM-immune is now a loop allowed to run 50 seconds by design, not a build that might hang — written in |
| `ledger.rs:804` | "the SIGTERM handler is armed *before* this call" | still TRUE |
| `PHASE-8-surface.md:1756` | "the refusal runs as the *first statement* in `serve()`" | still TRUE |
| `serve_pre_handshake_durability.rs` module doc | the window it probes | still TRUE; **extended** with the proxy case |
| `cli/serve_web.rs` `authorize_bind_web` | "mirrors `mcp::serve::authorize_bind`" | one clause added: it mirrors the rule, not the J2 section — a reader takes no lease and binds no endpoint |
| `I-observability.md:294-319` | history of the I-R2-1 arming move | left alone: narrative about I, not a claim about today |
| `main.rs:503` | `authorize_ledger` keeps the CLI's wording verbatim | untouched |

**Sweep 2 — the lease/endpoint schema claim-family (11 sites, 3 stale).**

| Site | Claim | Verdict |
| --- | --- | --- |
| this section's own bullet | the five-column list | **STALE by construction** — corrected above |
| `migrations/sqlite/001_init.sql` | DDL + header | updated |
| `migrations/cockroach/001_init.sql` | DDL + header | updated |
| `store/sqlite.rs` `INSERT INTO session_leases (…)` + its read-back | two duplicated column lists | updated, and collapsed into one `LeaseRowText` + one `LEASE_ROW_SQL` so they cannot drift in shape again |
| `store/cockroach.rs` same pair | same | same, via `LeaseRowTs` |
| `store/lease.rs` `LeaseInfo` docstring | "identity, timing and fencing token" | **STALE** — a three-item list over a four-member struct; corrected |
| `PHASE-8-surface.md:1215` | `LeaseHolder` / `LeaseInfo` member lists | **STALE** — annotated with the fourth member |
| `PHASE-8-surface.md:1219` | "three new methods" on `GraphStore` | **STALE** — annotated with `read_lease` as a fourth |
| `scripts/provision.sh:34` | enumerates the schema's **tables** | clean — a column is not a table |
| `scripts/loadtest/check_durability.py:136` | `SELECT holder, expires_at, current_token` | clean — named columns, not `SELECT *` |
| `docs/reference/*` + `site/src/content/docs/*` | four mirrors | clean — they carry **no** lease-schema claim at all |
| `t8.8-surface-audit.md` doc-count table | "185 missing docs left" | clean — every new `pub` item carries rustdoc |

**Sweep 3 — "what happens to a second writer", run because the register rule
demanded it and neither planned sweep covered it (5 pairs, 1 falsified).**

| Site | Claim | Verdict |
| --- | --- | --- |
| `end-to-end.mdx` + its site mirror | "A second writer … is refused … **whether it arrives as another `serve`** or as a command line write" | **FALSIFIED** — a second stdio `serve` is exactly what J2 stopped refusing. Rewritten in both copies to split the two cases, with a link to the new mcp.mdx section |
| `skills/lambo-cloudops/SKILL.md:30` | "do not run two writer processes against one session" | **STALE as advice** — true about *writers*, wrong about *processes*. J1's sweep passed it as "still true because the lease is untouched", which it is; what changed is what a second process does. Narrowed to "two CLI writers", with the serve case stated |
| `api.mdx:125` + mirror | "A session has exactly one writer" | clean — the lease is untouched, and the proxy is not a writer |
| `cli.mdx:167` + mirror | a command line write is refused and names the holder | clean — CLI verbs are not serves and do not proxy |
| `mcp.mdx` + mirror | transports | **extended**, not corrected: a new "More than one client on one machine" section, byte-identical prose in both copies with each copy's own link convention |

**A correction for §J5, found while looking for the drift gate it asks for.**
§J5 says the four `docs/reference` ↔ `site/src/content/docs` files are "kept as
**byte-identical** pairs with no drift gate". The second half is true; the first
is **already false at HEAD**, and deliberately so. `cli.mdx` differs on 16 lines
and `mcp.mdx` on 43: the site copies carry Astro component imports, rewrite every
internal link to a `/lambo/...` prefix, and `mcp.mdx`'s site copy has a whole
"Verified clients" section the docs copy does not. So the one-line `diff` gate
§J5 proposes would be **red on the day it landed**, and J2 did not add it. What
J5 actually needs is a gate over the shared prose with link prefixes normalised
and site-only sections excluded — a different and larger job than a `diff`, and
worth costing before it is promised.

### J2 round-1 review remediation

Reviewed **REQUEST_CHANGES** at `bbac803` — one P1, seven P2, thirteen P3
([adve-review-mooshik-J2-round1.md](../adversarial-review/adve-review-mooshik-J2-round1.md)).
All twenty-one are closed in four commits (`58faeac` the P1, `fdb3225` the rest of
`proxy.rs`, `8daf389` `endpoint.rs`, and this note), plus a fifth for the two P2s the live
two-client probe found in the same code (J2-L1, J2-L2). Nothing is carried. The design is
untouched: the byte pipe, the wedge invariant, the derived address and the declared
deviation all stand — the blockers were gaps between what the design argued and what the
code did.

* **J2-R1-1's live confirmation.** The probe reproduced the P1 against `bbac803` with a
  20s-delayed embedder to widen the window: the holder was killed mid-embed, the proxy logged
  the closed connection in **18ms**, sent the client **nothing**, and the call hung for the
  client's full 120s timeout before failing. Exactly the finding's prediction, and exactly what
  `58faeac` turns into an immediate honest error. The idle-death path (no call in flight) was
  already honest and took ~2s.

* **J2-R1-1 (P1, the blocker) — an in-flight forwarded request was never answered when the
  holder's connection closed.** `HubProxy::run`'s own docstring promised "every forwarded
  call fails honestly and immediately (never hangs)" (round 3 re-derived that sentence — the
  three bounds it rounded to "immediately" are now written out, J2-R2-1); the pump only
  answered frames whose
  *write* failed. A frame written successfully and then lost with the holder got no reply
  and no error — and the wedge was permanent rather than transient, because the reconnect
  lives in the `client_rx` arm: a client politely awaiting its response sends nothing, so
  the proxy never reconnects either. The generation filter was the other half of the same
  hole, dropping both a late response from a superseded connection *and* that connection's
  `Closed`. The reviewer reproduced the whole sequence from unmutated pump code.

  The pump now tracks every forwarded request id, tagged with the hub connection it went
  out on; a genuine response (a `result` or `error`, no `method`) retires its id from any
  generation, and a connection's `Closed` answers every id still outstanding on it. **The
  text is a different text and the code is a different code**: `HUB_UNREACHABLE_MESSAGE`
  says "NOTHING WAS READ OR WRITTEN", which is true of a frame that never left this
  process and false of one that reached the holder, so in-flight loss gets its own
  `-32002` and its own wording — the outcome is *unknown*, and the one instruction that
  resolves it safely is *recall before re-deriving*. A caller can tell "did not happen"
  from "nobody knows", which is the only thing it can act on.

  **Nothing is retried, argued rather than assumed.** Retrying idempotent reads needs this
  process to know which calls are idempotent — parsing `params.name` and knowing what the
  seven tools do, i.e. exactly the tool-level understanding the byte pipe is chosen not to
  have. Nor would it be cheap: a reconnect can only succeed once a new holder exists, up to
  `LEASE_TTL + ELECTION_SLACK` away, so "retry the read" means holding the caller open for
  the better part of a minute — the hang J2 removes, reintroduced for the calls least in
  need of it. Reconnect stays lazy for the same reason: the client now *has* its answer, so
  its next request drives it.

  Pinned by `a_call_in_flight_when_the_holder_dies_is_answered_rather_than_lost`, which
  drives a real `lambo serve` subprocess as the proxy against a hand-written holder that
  answers `initialize` and then drops the connection on the first tool call — the
  reviewer's scenario, deterministic, no signal race. Against the pre-fix pump it fails at
  the harness's 30s bound; with the fix the whole test is 1.2s.

* **J2-R1-2 (P2) — `store_identity` hashed the store's *spelling*.** `path = "./lambo.db"`
  is what every published example shows and `SqliteConnectOptions` resolves it against each
  process's own cwd, so two clients launched from two directories were two SQLite files
  with **one** derived socket: two leases, and the second holder's `AddrInUse` branch
  unlinking the first holder's live socket, after which a proxy of graph A forwarded writes
  into graph B. The path half is now canonicalized first. Symlinks are **resolved**, so one
  store reached two ways is one socket; a file that does not exist yet resolves through its
  parent directory (the common case, since `SqliteStore::connect` builds a *lazy* pool); a
  URI spelling is left verbatim, because `cwd.join` on a URI would make one store's
  identity cwd-dependent — the same bug from the other side.
* **J2-R1-3 (P2) — the endpoint directory lost its per-uid discriminator, and the mode
  check followed symlinks.** This one was **graph↔code drift**: the decision recorded in
  the project graph was `$TMPDIR/lambo-<uid>` / `/tmp/lambo-<uid>`, the code shipped
  `.../lambo`, and this note was written to match the code. Without the suffix the first
  uid to run `lambo serve` creates `/tmp/lambo` at 0700 and every other uid's bind fails
  `EACCES` — a cross-user lockout on a case that worked before J2, with `/tmp`'s sticky bit
  preventing the second user from clearing it. Restored on both fallbacks (not on
  `XDG_RUNTIME_DIR`, which already carries a uid and would cost `sun_path` bytes for
  nothing). The mode check moved to `symlink_metadata` and gained an ownership assertion:
  `std::fs::metadata` followed a pre-placed `/tmp/lambo-<uid> → /tmp/theirs`, and a 0700
  directory owned by *another* uid that we can still write into passed too. The graph now
  carries the correction, so graph and code agree again.
* **J2-R1-4 (P2) — a torn final frame was forwarded as a complete one.** `tokio::io::Lines`
  yields an unterminated remainder as a line, so the half of a JSON object that reached the
  socket before a holder died was delivered to the client's MCP wire as a frame. Replaced
  by one bounded, resynchronising `read_frame`, which also closes J2-R1-17 (a non-UTF-8
  byte ended the pump) and J2-R1-18 (no length cap) — three findings, one reader. A torn
  frame is dropped with a WARN and followed by `Closed`; an oversize or non-UTF-8 frame is
  dropped *through its newline*, so the stream survives.
* **J2-R1-5 (P2) — the over-long `sun_path` refused the whole serve while a failed bind
  degraded.** The harsher outcome sat on the cheaper problem, and made a long `TMPDIR` a
  hard startup failure on a machine that served fine before J2. `for_store` now returns
  `Option` (there is no failure left to report), logging the reason at ERROR. Pinned at the
  binary with an 80-character `XDG_RUNTIME_DIR` (the only environment variable left in the
  derivation after J2-L1): the client's session works and the lease row's `endpoint` is NULL,
  which is the honest row for a holder nothing can reach.
* **J2-R1-6 (P2) — the reconnect's re-read was unpinned and its stated reason was false.**
  It said the address is never cached "because a new holder is a new endpoint"; the address
  is a *pure function* of session and store, so every holder binds the same path — which is
  why `bind` needs a stale-socket branch at all. The real value is liveness and honest
  errors: the row is the authority on whether there is a holder. Docstring and test comment
  rewritten. The pin needed a window the review's prescription did not have — at the
  existing release point the proxy's writer is live, so `dial` is never entered — so it is a
  separate test that creates the discriminating state (writer `None`, a live listener, no
  row) and is red under the reviewer's exact `dial()` mutation while the other three tests
  in the file stay green.
* **J2-R1-7 (P2) — sweep 1 missed the family's central noun.** Sweep 1's table above is
  corrected rather than left claiming a result the tree no longer matches. All nine sites
  rewritten; `build_memory` kept and docstringed as the library-only entry point it now is
  rather than deleted, because deleting a `pub`, re-exported API is a wider act than a
  remediation should take in passing — noted as a candidate for J5. The better argument the
  review found unwritten is now written at `serve.rs`'s arming comment: arming above the
  attach would no longer risk deferring a signal past a build that *might* hang, it would
  make a loop that is *designed* to run up to 50 seconds unkillable for all of it.
* **J2-R1-8 (P2) — the replay swallow was an unbounded read inside a `select!` arm body**,
  so a holder that accepted and never answered parked the pump and made the process deaf to
  SIGTERM. Bounded by `CONNECT_BUDGET` and `MAX_REPLAY_FRAMES`, with the consequence stated
  at the constant: worst-case deafness inside one arm body is 2 × `CONNECT_BUDGET`, bounded
  and written down. **Deviation, argued:** the review's stronger option — hoist the
  reconnect out of the arm body behind a `pending_frame` state machine — is not taken.
  Every `send` in this pump is also awaited in an arm body, which is what keeps writes from
  being torn by cancellation (an attack the review checked and cleared), so arm-body awaits
  do not go away; only the *unbounded* one was the defect.

  **Round 3 overturned the number and with it the deviation (J2-R2-1).** "2 ×
  `CONNECT_BUDGET`" was true of `connect` and `replay` and false of the arm body, which also
  contains `dial()`'s opening `store.read_lease` — bounded by nothing here, and in practice
  by sqlite's 8s `busy_timeout` or cockroach's 20s `statement_timeout` *behind* sqlx's
  30s default pool acquire, i.e. ≈38s and ≈50s. The deviation had been argued against 4s.
  Re-made at the real number: the dial is now raced against the shutdown future and capped
  at a chosen `DIAL_BUDGET` (6s). See the round-3 section below.

**Two further P2s, from the live two-client probe** (real products — `cursor-agent` 2026.08.11
as the holder side, `opencode` 1.18.18 as the proxy side — against `bbac803`; timeline and
per-run logs were produced by the probe agent). Both are in the code this remediation was
already editing, and both are the *same* defect seen twice: a number or an input chosen from
lambo's own internals when it had to be chosen from what a real client does.

* **J2-L1 (P2) — endpoint derivation depended on client-inherited environment, so two client
  products derived two directories for one session on one store.** Measured:
  `cursor-agent` **scrubs** `TMPDIR` from the environment of the MCP server it spawns (so the
  derivation fell through to `/tmp/lambo`), `opencode` **passes** macOS's per-user `TMPDIR`
  through (`$TMPDIR/lambo`). Same binary, same store, same session, two addresses. The losing
  serve compared the row's published endpoint against its own derivation, refused ("it is
  running a different endpoint scheme … could reach a socket serving a different graph"),
  waited out its 50s budget, and `opencode` declared the server failed at 31.96s. Net:
  cross-client memory silently absent on **unmodified default wiring** — precisely the outage
  J2 exists to remove, arriving through a door J2 opened.

  **Both halves of the fix, because they answer different things.** (1) `TMPDIR` is out of the
  derivation: it is a per-child, per-client-product variable, which is the one kind of input a
  *shared* address may not have, and losing it costs nothing — `/tmp/lambo-<uid>` is shorter
  than macOS's `TMPDIR`, so the `sun_path` headroom goes from 6–9 bytes to 47–50.
  `XDG_RUNTIME_DIR` stays: it is set once per login session by the platform rather than per
  child by a client, and it is the only rung that works where `/tmp` is not shared. (2)
  `proxyable` now compares the address's **file name**, not the whole path, and returns the
  path to dial. The name is a hash of the session and the *canonicalized* store identity — so
  J2-R1-2 is what makes it trustworthy — and it therefore decides *which graph*, while the
  directory decides only *reachability*. A matching name in an unexpected directory is benign
  by construction and is dialled, with an INFO line naming both paths; a differing name is the
  real different-graph case and is still refused. Half (1) removes the observed incidence,
  half (2) removes the class, including `XDG_RUNTIME_DIR` being scrubbed by one client and not
  another.

  **The trust boundary is unchanged: the store.** The published path is store data, so a
  writer who could forge it could already write graph content the model reads, which is
  strictly more power. One hardening rides along for symmetry: the private-directory check
  (not a symlink, owned by this euid, mode 0700) is factored out of `bind` and now runs on the
  **dial** side too, so a directory this process would refuse to place a socket in is one it
  refuses to reach a socket in.

  Pinned three ways: two unit tests over `proxyable` (a matching name in each of the two
  directory shapes the probe actually produced → the published path; a differing name, an
  altered hash, and a nameless path → refusal), one that `TMPDIR` is no longer an input at
  all, and an integration test that spawns a real serve against a holder listening in a
  directory that serve would never derive. Red under the mutation that restores whole-path
  comparison — both the unit test and the integration test.

* **J2-L2 (P2) — the election budget was a lease number where it had to be a client number.**
  It was `LEASE_TTL + ELECTION_SLACK` = 50s, reasoned entirely from the lease: a holder that
  stops heartbeating loses its row within one TTL, so one TTL plus slack either finds a live
  hub or wins the lease. Sound about the lease, wrong about the client — an MCP client that
  waits too long does not report "starting", it reports **failed**, and a failed server has no
  tools at all. Measured: `opencode` gave up at 31.96s.

  `ELECTION_BUDGET` is now 20s, documented at the constant as a client-tolerance number that
  must **not** be re-derived from `LEASE_TTL` — the two answer to different constraints and the
  lease's is the one that may not move. And nothing waits blindly any more: the row carries
  `expires_at`, so each pass asks whether the lapse falls inside the budget that is left, and
  refuses **immediately** with the seconds named when it does not. That is strictly better
  than the old behaviour in both directions — the cases that can succeed inside a client's
  patience still do (a client that starts ~17–32s after a holder's death finds under ~13s of
  lease left and waits it out; the probe's own dead-holder election took 10.2s), and the
  cases that cannot now say so in milliseconds with
  an actionable number instead of spending the client's whole startup gate to reach the same
  refusal. Pinned at the binary: a CLI-shaped holder with a full TTL left is refused in under
  10s; under the mutation restoring the 50s budget and removing the arithmetic, the same test
  measures **45.1s**.

  **What this does not fix, stated (corrected in round 3 — J2-R2-2).** *Any* abrupt holder
  death leaves 30–45s of lease, not just one "immediately after a heartbeat": refreshes come
  every `LEASE_HEARTBEAT_INTERVAL` = 15s and each sets `expires_at = now + LEASE_TTL` = 45s,
  so the remaining lease is always in `[LEASE_TTL − LEASE_HEARTBEAT_INTERVAL, LEASE_TTL]`,
  and the election refuses above ~13s. So a client starting promptly after an abrupt death is
  refused **always**, by design, with the retry interval named — and "the client's next start
  succeeds" was false as written: at kill+5s about 33s of lease remains and the next start is
  refused too. The refusal's own "Retry in 39s" is the honest number. Removing the case
  needs the wait not to block the MCP `initialize` response at all, which means serving a
  client from a role that can still become a holder — in-process promotion, which §J2 scoped
  out with an argument this remediation does not reopen. Pinned by
  `an_abrupt_holder_death_outlasts_the_election_budget` over the extracted `waiting_fits`
  predicate, so the arithmetic is asserted rather than described.

**The thirteen P3s**, all closed: `unreachable_reply` now requires a `method`, so a client
*response* frame is never answered with an error keyed to the holder's own request id
(J2-R1-10); the lost post-initialize session state is stated at `Handshake` with the
argument for documenting rather than enumerating the protocol (J2-R1-11); the swallow is
id-matched and forwards what came before it (J2-R1-12); `read_lease`'s default `Ok(None)`
now says a real adapter must override it and what the silent failure looks like (J2-R1-13);
`HUB_UNREACHABLE_MESSAGE`'s "45 seconds" is pinned by a `const` assertion against
`LEASE_TTL` (J2-R1-14); the vacuous handshake test now drives `replay` and asserts no bytes
were written (J2-R1-15); the verbatim-`agent_id` claim is pinned with `agent-b `, an id a
trimming forwarder would break (J2-R1-16); the four collapsed message literals are
re-broken and two tests assert both a phrase spanning a continuation and the absence of a
double space (J2-R1-9); `biased` now covers only the shutdown arm, with the two traffic
directions nested in an unbiased inner `select!` (J2-R1-21); and the endpoint's operational
surface — where the socket lives, the directory rule, the two env vars, and when it is safe
to delete — is one paragraph in `mcp.mdx` and its site mirror, byte-identical in both
(J2-R1-19).

**J2-R1-20 — the cockroach count, resolved: `524/0/0` *was* right.** It reproduces exactly
at `bbac803` with `cargo test --no-default-features --features store-cockroach` — no
`--lib`, no `embed-fixture`. The reviewer tried four other invocations and none of them was
that one, which is the whole finding: the count was never the problem, the missing
invocation was. Recorded here so the next reviewer does not have to guess.

**Gate invocations, with the count each one produces at the remediation head.** Run in the
`wt/j2` worktree with a shared `CARGO_TARGET_DIR`.

| Invocation | Count |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-cockroach,embed-fixture -- -D warnings` | clean |
| `cargo test --all --features fixtures` | 853/0/3 (832 at `bbac803`) |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 940/0/3 (914 at `bbac803`) |
| `cargo test --no-default-features --features store-cockroach` | 545/0/0 (524 at `bbac803`) |
| `bash scripts/observability/verify.sh` | ALL CHECKS PASSED |

Counts are the sum over every test binary the invocation runs, which is why they exceed the
`--lib` figures a per-crate reading gives.

### J2 round-2 review remediation

Against `adve-review-mooshik-J2-round2.md` at `920e096` — **REQUEST_CHANGES, 2 P2 / 5 P3**,
with all 23 round-1 and live-probe findings verified closed at the artifact. The reviewer
framed **both** P2s as doc-precision. The operator overturned that for the first one, and the
ruling is the interesting part of this round.

**J2-R2-1 (P2) — the SIGTERM-deafness bound. Operator ruling: a code fix, not a doc fix.**

The finding: `CONNECT_BUDGET`'s docstring claimed the pump is deaf to SIGTERM inside one
`client_rx` arm body for at most 2 × `CONNECT_BUDGET` = 4s. True of `connect` and `replay`,
false of the arm body, which also contains `dial()`'s opening `store.read_lease` — under no
budget of this module's at all. The reviewer prescribed naming the store timeouts in the
sentence. The operator declined that: **the 4s figure is the number the round-1 deviation
used to decline the reconnect hoist**, and a serve deaf to SIGTERM for tens of seconds is the
same defect class three I rounds were spent closing. Re-make the decision at the true number,
and make the worst case a *chosen* constant rather than an emergent store timeout.

*The true number, re-derived from both adapters at source* (and it is worse than the review's
estimate, because neither adapter overrides sqlx's pool acquire):

| Term | sqlite | cockroach |
| --- | --- | --- |
| pool acquire | 30s (sqlx 0.8.6 `PoolOptions::default`, not overridden; `max_connections(1)`, so a concurrent flush queues the read here) | 30s (same default; a lazy pool's acquire includes TCP connect + auth) |
| the statement | 8s `busy_timeout` (`SqliteStore::connect`) | 20s `statement_timeout` (`cockroach::STATEMENT_TIMEOUT`) |
| connect | `CONNECT_BUDGET + CONNECT_RETRY` + one attempt ≈ 2.1s | same |
| replay | `CONNECT_BUDGET` = 2s | same |
| **worst case** | **≈ 38s** | **≈ 50s** |

*The shape chosen, of the two the operator offered: **(b), the middle**, and deliberately not
(a) the full hoist.* The hoist would move the reconnect out of the arm body behind a
`pending_frame` state machine. Round 1's argument against it survives the new number
unchanged, because it was never about the number: **every `send` in this pump is also awaited
in an arm body**, and that is what keeps frames from being torn by `select!` cancellation. The
hoist removes one arm-body await and leaves the others, at the cost of a state machine in the
one loop J2-R1-1 exists to make reliable. What the new number *does* change is that the
unbounded await had to stop being unbounded — which needs neither the state machine nor the
restructuring:

* **Deafness is answered by racing, not by a budget.** `HubProxy::dial_bounded` polls the
  whole dial against the shutdown future, `biased`, shutdown first. A SIGTERM mid-dial is
  honoured at the next poll whatever the store is doing, so there is no longer a store
  timeout in the deafness path for a constant to under-state. The dial is the *only* arm-body
  await that can be abandoned at any instant without consequence: the connection it is
  building belongs to nobody yet, so a torn `initialize` goes into a socket closed in the same
  statement. The frame writes do not have that property and are still un-raced.
* **The client's wait is answered by a chosen constant.** `DIAL_BUDGET` = **6s** caps the
  whole dial — row read, connect, replay. Chosen from both directions: above the sum of the
  budgets the dial's own steps carry (`2 × CONNECT_BUDGET + CONNECT_RETRY` ≈ 4.1s, pinned by
  a `const _: () = assert!`, which fails the build at 4s), so a healthy-but-slow holder is
  never cut off by the outer cap and each inner step still raises its own better-attributed
  error; and below the smallest store-emergent bound in the table (sqlite's 8s
  `busy_timeout`), so the number an operator reads at the constant is the number that
  governs. Past it the call is answered with `HUB_UNREACHABLE_MESSAGE` and the next call
  re-reads the row.
* **The first dial moved inside the raced region too.** `run` awaited it *above* the
  `tokio::pin!`, so a proxy was deaf for a whole store timeout before its loop ever started —
  the pre-handshake shape of the same defect. `tokio::pin!` now precedes it and a shutdown
  there returns `Ok(())`.

*Pinned, red-first, three tests, all on a paused clock so they cost no wall time.* The seam is
an injected `GraphStore` whose `read_lease` never returns (`HungLeaseStore` — the `BatchSink`
precedent: inject at the trait the production type already takes, rather than reproducing a
store's configuration).

| Test | Mutation | Result |
| --- | --- | --- |
| `a_shutdown_during_the_dial_is_honoured_and_not_left_to_the_store` | drop the `biased` shutdown arm | **red**, cleanly — the cap still returns, so it fails on the variant *and* the elapsed time rather than hanging |
| `a_shutdown_during_the_proxys_first_dial_exits_cleanly` | same | **red** |
| `a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget` | `DIAL_BUDGET` → 3600s (i.e. "let the store decide") | **red** — on the `< 8s` ceiling assertion (the wait itself completes instantly under `start_paused`, so the "waited …s" message is not what fires; corrected J2-R3-2) |
| the `const _: () = assert!` | `DIAL_BUDGET` → 4s | **build fails** with the assertion's own text |

The reviewer's `proxy.run(std::future::pending())` negative control is untouched and still
red: with a shutdown future that never completes, nothing in this change makes the proxy exit.

*What is still unbounded in the arm body — a NEW residual, stated rather than hidden.* The
frame writes that share it — six `Self::send` sites writing to the holder or to the client's
stdout (count corrected J2-R3-3) — are neither raced nor budgeted, because a write abandoned
mid-frame delivers a torn JSON line, which this pipe may never do. Each is bounded by its peer
draining the socket. That is a different shape from the store read: a peer that never reads is
itself already wedged, whereas a row read stuck behind a flush at the pool wedged a **healthy**
proxy talking to a **healthy** holder. One of the six is `answer_lost`, whose burst length is
the `inflight` list J2-R2-7 declined to cap — the two residuals are coupled, and J3's receipt
ceiling bounds both. Abandoning a client-facing write is a behaviour decision of its own and is
not taken here.

**J2-R2-2 (P2) — "the majority of real cases … the wait still succeeds", refuted by the
tree's own constants.** Doc + test, as the reviewer prescribed and the operator confirmed;
the refuse-fast behaviour is correct and unchanged. `LEASE_TTL` = 45s and
`LEASE_HEARTBEAT_INTERVAL` = 15s, and every refresh sets `expires_at = now + LEASE_TTL`, so an
abrupt death leaves `[30s, 45s]` — never uniform on `[0, 45]`. The election refuses whenever
`lapses_in > ELECTION_BUDGET − ELECTION_SLACK` = 15s at best, ~13s once the attach attempt and
the probe have spent some budget. Every value in [30, 45] is above it, so a prompt restart is
refused **always, by design** — measured live at 2.12s with 40s of lease left. Three edits:
`ELECTION_BUDGET`'s docstring carries the derivation and the ~17–32s window that *does* wait;
§J2's residual says *any* abrupt death rather than "immediately after a heartbeat" and drops
"The client's next start succeeds" (false — at kill+5s about 33s remains and the next start is
refused too); and the Done-when box states the exclusion in the operator's words. Pinned by
`an_abrupt_holder_death_outlasts_the_election_budget` over a predicate extracted for the
purpose (`waiting_fits`) — red under the mutation restoring the 50s budget, which is exactly
the shape the claim would have been true for.

**The five P3s, all closed.**

* **J2-R2-3 — the refusal told an operator a `kill -9`'d holder "is still refreshing it"**,
  false half first, contradicted by its own next clause. The clause is `build_attach`'s and is
  contractually byte-identical to pre-J2 `build`'s, so it is not `serve`'s to reword — but the
  *composition* with the probe outcome is J2-L2's, and that is where it is repaired.
  `correct_the_refresh_claim` replaces the clause with "has not yet let its lease lapse — but
  its endpoint is not answering, so it has most likely died", **only** when the outcome is
  `ENDPOINT_NOT_ACCEPTING`; every other refusal is a live holder this process merely cannot
  forward to, and the clause is true for it. Both literals are now shared constants
  (`memory::STILL_REFRESHING_CLAUSE`, `serve::ENDPOINT_NOT_ACCEPTING`) rather than duplicated
  strings, so a reword at either end cannot silently turn the correction into a no-op. Pinned
  at the unit level and at the binary
  (`a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing`, a published endpoint
  that was never bound), with the *narrow* half asserted in
  `a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once`: that holder is a live
  CLI verb, and it must keep saying "is still refreshing it". The unit test also carries
  J2-R1-9's two message rules on the new literal (no double space, and the phrase spanning
  the continuation survives it) — which is not decoration: it caught a genuinely broken
  string continuation in `PROBABLY_DEAD` before this commit, rendering "is not      answering"
  in an operator-facing refusal.
* **J2-R2-4 — the headline "proxying to the session holder" line named the derived address,
  not the dialled one.** Under J2-L1 half (2) those differ by construction, so the line an
  operator greps for named a file that does not exist. `dial` now returns the address it
  connected to, `Dialled::Hub` carries it, and both the headline line and the reconnect line
  log `dialled=` and `derived=`, named for what they are.
* **J2-R2-5 — the dangling-symlink window.** `canonical_store_path`'s "the same store reached
  by a symlink and reached directly derives **one** address" is false while the target does not
  exist: `realpath(3)` fails with `ENOENT` on a dangling link, so the link resolves to its own
  name, `create_if_missing` then creates the target, and the next process resolves to the
  target — one store, two identities, one on each side of the file's creation. Claim narrowed
  with the mechanism and the consequence (a `proxyable` refusal whose message blames three
  things that are not true; safe, because the lease still serialises the writers). Not closed:
  a hand-rolled link-chain resolver beside `canonicalize` is a second path resolver for a
  configuration no documented wiring produces.
* **J2-R2-6 — `proxyable` accepted a relative or bare published path**, which `dial_dir` then
  turned into `parent() == Some("")` (an empty path in an operator-facing message) or `.`
  (this process's cwd). One line: `published.is_absolute()` or `EndpointIsNotOurs`. The trust
  boundary is unchanged and is still the store; this is the directory check not being handed a
  relative path. Both spellings added to
  `a_holder_publishing_a_different_address_name_is_not_proxyable`, red without the check.
* **J2-R2-7 — the unbounded, linearly-scanned `inflight` list. Deviation, argued: documented
  with its real ceiling, not capped.** Growth and the O(n) scan are bounded by the same real
  quantity — the client's own in-flight window, and the client is the local, trusted party
  that spawned this process. It is *not* `MAX_FRAME_BYTES`'s shape: that grew on bytes a peer
  chose to send with no reply expected. A cap was considered and declined for a specific
  reason — answering the oldest id early to make room lets the holder's real answer arrive
  afterwards, match nothing, and be forwarded as a **second** response to an id the client has
  already been given an error for, a protocol violation manufactured to fix a growth with no
  real cause; tearing the connection down instead puts a new failure mode into the path
  J2-R1-1 exists to make reliable. What is added instead is the observability the argument
  depends on: `INFLIGHT_DEPTH_WARN` = 64 (two orders above the claimed ceiling, so it cannot
  fire on real traffic) logs once if the ceiling ever stops holding. **J3 is the reason that
  matters** — receipts lengthen how long an entry stays outstanding, so the ceiling should be
  re-derived there rather than inherited.

**Register sweep (claim-family rule; the false-stated-reason family is J2's recurring
defect — this is instance four, five and six).** Per file, including the nulls:

| File | Swept for | Result |
| --- | --- | --- |
| `src/mcp/proxy.rs` | every numeric claim in the arm-body neighbourhood, re-derived from the constants | 4 corrected: the `2 × CONNECT_BUDGET` bound itself; `connect`'s own bound (`CONNECT_BUDGET + CONNECT_RETRY` + one attempt ≈ 2.1s, not 2.0s); `run`'s "fails honestly and **immediately**", which rounded three different numbers to one word (now written out — µs for a lost in-flight call, ≈2.1s for a dial onto a refusing socket, `DIAL_BUDGET` for the whole dial); "an honest error in **microseconds**", where the live measurement is 2.6 ms |
| `src/mcp/proxy.rs` | premises invalidated by the race | 1 corrected: `Handshake::replay`'s "so the shutdown branch cannot be polled while this runs" — the premise the budgets-only argument rested on, now false and replaced by what is true |
| `src/mcp/serve.rs` | the election's distribution claim and everything leaning on it | **2 corrected.** `ELECTION_BUDGET`'s parenthesis and its conclusion; and `ELECTION_SLACK`'s "absorbs store-clock skew and **one missed refresh interval**" — a second false-stated reason found by the sweep and not by the review. 5s is not one refresh interval (`LEASE_HEARTBEAT_INTERVAL` is 15s, three times it), and a missed interval needs no absorbing: a holder gets three chances inside one TTL and is *supposed* to lose the lease if it misses all of them. What 5s actually covers is the race between reading the row and acting on it. The `resolve_role` "what it waits for" section and the 31.96s / 20s / 50s figures re-checked and **still true** |
| `src/mcp/endpoint.rs` | the canonicalisation rule's four bullets | 1 narrowed (symlinks); the not-exists, neither-resolves and URI bullets re-read and **still true** |
| `src/memory.rs` | the "byte-identical to what `build` returns" claim, after extracting the clause to a const | **still true** — the const preserves the bytes exactly; nothing else stale |
| `tests/serve_proxy_multi_client.rs` | test docstrings whose measured numbers my edits touch | **nothing** — the 50s / 31.96s / 45s figures are all about the election, which did not change |
| `dev-diary/lambo-for-mooshik/J-multi-client.md` | the two P2 claim families plus the docstring it quotes verbatim | 4 corrected: the round-1 record's quotation of `run`'s docstring (now past tense, with a pointer); the J2-R1-8 entry's `2 × CONNECT_BUDGET` and the deviation it justified; the uniform-expiry parenthesis; the "dies immediately after a heartbeat" residual and "The client's next start succeeds" |
| `dev-diary/lambo-for-mooshik/J-multi-client.md` | the Done-when boxes | 1 amended: the `[~]` unclean-kill box now states the prompt-restart exclusion |
| `docs/`, `AGENTS.md`, `README.md`, `dev-diary/PHASE-8-surface.md`, `dev-diary/notes/remediation-tasks.md` | `rg` for `CONNECT_BUDGET`, `DIAL_BUDGET`, `ELECTION_BUDGET`, `four seconds`, `uniformly`, `next start succeeds`, `never hangs`, `canonical_store_path`, `proxyable`, `inflight`, `does not lapse for`, `NO TOOLS`, `proxying to the session holder` | **nothing** — not one of the changed claims is mirrored outside the three source files and this phase doc |
| `docs/reference/cli.mdx`, `docs/reference/end-to-end.mdx` | `is still refreshing it` — the clause J2-R2-3 corrects | **checked, deliberately NOT changed.** Both are `lambo derive` transcripts, where the message comes from `build`'s `LamboError::Conflict` with no endpoint probe anywhere near it. A CLI verb that lost the lease genuinely is talking about a live holder, so the clause is true there. The correction is `serve`'s election composition only, which is what J2-L2 introduced and what `correct_the_refresh_claim`'s narrowness test pins |

**Gate invocations, with the count each one produces at this remediation's head.** Baselines
are `920e096`, the review head. Run in the `wt/j2` worktree with a shared `CARGO_TARGET_DIR`.

| Invocation | Count |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-cockroach,embed-fixture -- -D warnings` | clean |
| `cargo test --all --features fixtures` | 858/0/3 (853/0/3 at `920e096`) |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 946/0/3 (940/0/3) |
| `cargo test --no-default-features --features store-cockroach` | 550/0/0 (545/0/0) |
| `bash scripts/observability/verify.sh` | ALL CHECKS PASSED (40 ok) |

Every delta reconciles to a named test. **+5 lib tests**, which is the whole cockroach and
`--all` delta: `a_shutdown_during_the_dial_is_honoured_and_not_left_to_the_store`,
`a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget`,
`a_shutdown_during_the_proxys_first_dial_exits_cleanly`,
`an_abrupt_holder_death_outlasts_the_election_budget`,
`a_dead_holders_refusal_does_not_claim_it_is_still_refreshing`. **+6 on sqlite**: those five
plus `a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing`, which needs a
real store. No test was renamed or removed;
`a_holder_publishing_a_different_address_name_is_not_proxyable` gained two cases inside one
existing test, which is why J2-R2-6 shows no count change.

## J3 — Writes acknowledged before the embedder

**Status: implemented — see [§J3 Status](#j3-status--landed) below** for what
shipped, the measured latency, the deviations argued (fetch-by-id on
`lambo_stats` rather than an eighth tool; the declared metric-2 regression), and
every constant's derivation. The spec below is the spec it was built against.

A warm `lambo_derive` is 27ms, of which 22 to 25ms is the embedding call. Durability is
*already* async: write-behind returns long before anything reaches disk. The wait buys the
agent nothing it is waiting for.

**Rule:** a write may be acknowledged asynchronously when its result does not gate the
caller's next action. `lambo_derive` and `lambo_record_action` qualify. **`lambo_reserve`
does not** — its result *is* the caller's next action, and an async reservation has two
agents editing while each believes it holds the lock.

**Shape:**

1. **Synchronous validation pre-pass, then ack.** `derive` already validates in a read-only
   pre-pass needing no embedding; keep it on the call path so common errors still surface
   at call time. This keeps one synchronous hop, since validation resolves against the
   graph and a proxy holds none. Async removes the embedder wait, not the round trip, and
   at 0.4ms the round trip is not worth removing.
2. **Embed, canonicalize and insert in the background** through the ordinary path.
3. **The ack carries a receipt id.** Outcomes are stored against it and delivered two ways:
   piggybacked on that agent's next tool response (tagged, so self-identifying), and
   fetchable by id. Not an MCP notification as the mechanism — notifications land in a
   client log rather than the model's context, repeating the exact failure this workstream
   exists to fix. The receipt doubles as **opt-in synchrony**: an agent that needs its write
   applied waits on the receipt, restoring read-your-writes on demand without charging
   every agent for it. No `await` flag needed.

**Dedup is unaffected.** Embedding precedes insertion either way, so the vector is present
when matching happens.

**Constraints on the background path:**

* **Backpressure.** Async adds no capacity, and batching does not help on this rig. The
  queue bound must be derived from a ceiling **measured on the deployment's own embedder**,
  never a constant: a hosted or GPU embedder may be slower per call while parallelising far
  better, at which point batching pays and this rig's result inverts. Drop policy is fixed
  regardless — bound, drop, log once, count in `lambo_stats`.
* **Per-agent FIFO.** Derive chains `Temporal` edges between consecutive interactions;
  out-of-order draining corrupts the chain. Interleaving across agents is fine.
* **Receipts:** expired must not read as unknown, and restart-lost must not either.
  Per-agent scoping is why this depends on J1.
* **The crash window widens but is not new** — the write-behind tail already dies with a
  crashed holder today.
* **The `ledger_queued_lines` arithmetic must be re-derived, not assumed.** Its gauge is
  `accepted − written − write_failed`, and that formula is correct *only because*
  `try_send`'s `Err(Full)` arm never touches `accepted` — the whole exclusivity argument
  is pinned by a test the alternative formula fails (see I-observability.md's Handoff
  Log). A receipt queue is a second bounded channel with its own accept/reject
  accounting; if receipts ride the ledger's counters, or the background path adds a
  third drop class, the shared subtraction in `Ledger::shutdown` and `queued()` —
  deliberately one expression so the live gauge and the exit count cannot drift — stops
  being obviously right. Re-derive it against the new counter sites;
  `adve-review-mooshik-I-round3.md`'s flip D is the map for the never-`accepted`
  classes.

### J3 Status — landed

**Status: implemented on `wt/j3`, awaiting review.** Four staged commits
(`427fabf` pipeline, `dcf29de` MCP surface, `f9abfbb` tests, `3e1bea4` two
defects found by measuring the binary), plus this note.

#### The latency claim, measured

Measured 2026-08-20 at the **release binary** over real stdio MCP, SQLite store,
one concept per call, 20 calls after a warm-up call, median of the per-call wall
clock as a client sees it. `base` is `166a3c8` (pre-J3, synchronous); `J3` is
this branch. The real embedder is the rig's live llama.cpp BGE-M3 q8_0 on CPU at
`127.0.0.1:8080`.

| Embedder | base median | J3 median | J3 min / max |
| --- | --- | --- | --- |
| `FixtureEmbedder` (no work) | 0.101 ms | 0.076 ms | 0.040 / 0.160 ms |
| **BGE-M3 q8_0 on CPU (live)** | **14.111 ms** | **0.048 ms** | 0.033 / 0.093 ms |

The claim is verified in the strong form: with the real embedder, J3's ack
(0.048 ms) is *at or below* the fixture-embedder ack (0.076 ms), which is what
"the embedder is no longer in the call" means — the ack is indistinguishable
from a call that does no embedding at all. Both runs applied all 21 concepts
(`write_queue_applied: 21`, `concept_count: 21`), so the fast number is not a
fast no-op.

Two figures differ from §Measurements and the difference is the rig, not J3:
that table's 27 ms warm derive was measured through the dogfood client stack on
the pinned `3039b82`, while this is a release binary driven by a raw pipe against
an already-warm llama-server. The *delta* is what J3 owns.

Probe output from the same runs, which is where the queue bound comes from:

| Embedder | measured `items_per_sec` | bound | clamped? |
| --- | --- | --- | --- |
| BGE-M3 q8_0 on CPU | 131.9 (110–141 across repeats) | 264 | no |
| `FixtureEmbedder` | ~21 000 (98 000 in one run) | 1024 | yes |

#### Design decisions

* **Queue at `Memory` level, delivery at MCP level.** Only `Memory`'s worker can
  produce an outcome; only the server knows how to render one to a model. The
  synchronous `derive` / `record_action` are **unchanged** and the async path is
  additive (`derive_async_as`, `record_action_async_as`) — making the existing
  surface async would silently remove read-your-writes from every embedded
  owner and every derive-then-assert test, a far larger change than J3 is.
* **The interaction is opened on the call path.** `begin_interaction_as` is
  synchronous and cheap, so submission order *is* `Temporal`-chain order by
  construction — strictly stronger than ordering the drain, because the chain
  stops depending on drain order at all. Per-agent FIFO lanes are still enforced
  in the drain, because insertion order decides which of two identical concepts
  is `created` and which is `matched`, and the receipt reports that.
* **Fetch-by-id lives on `lambo_stats`, not on an eighth tool.** This deviates
  from the shape sketched above and the reason is the register, not taste: spec
  §6.2 enumerates exactly seven tools, two tests pin that list with messages
  saying an eighth is a spec change, and "seven tools" is asserted in 40+ places
  including `evidence/` files that are *records of past runs* — rewriting those
  would falsify history rather than update a register. §J3 asks for outcomes
  "fetchable by id", not for a tool. `lambo_stats` is already the introspection
  surface, already grows this change's queue keys, and a receipt is the per-write
  grain of exactly the question its `flush_lag` / `log_depth` answer at session
  grain. `receipt` + `wait_ms` are its two new parameters; `wait_ms` clamps
  rather than refuses.
* **Receipt ids are self-describing** — process epoch, issue time, sequence — so
  `expired`, `restart_lost` and `never_issued` are distinguishable with no
  history kept. Eviction is oldest-first, so an evicted id is older than
  everything retained and `expired` is the honest answer for it too: eviction
  collapses into expiry instead of becoming a fifth class. Seven states, none of
  them "unknown". `restart_lost`'s wording is kept word-for-word consistent with
  the proxy's `HUB_LOST_CODE` (-32002): outcome UNKNOWN, recall before
  re-deriving.
* **`close()` quiesces the queue before it takes the writers gate**, and that
  order is forced rather than chosen: the gate's write side is held for the rest
  of `close`, so a worker passing through the gate could never finish and a
  `close` waiting for it would deadlock. Workers therefore never touch the gate;
  latching `closed` stops new jobs and the quiesce stops the workers. Anything
  past the budget is abandoned — aborted **and joined**, because aborting alone
  proves nothing (the R3-1 lesson) — and settled `failed` with a session-closed
  reason rather than left `pending` forever in an exiting process.
* **The daemon's wake is unchanged**: a background write pokes it through the
  same `Notify` the synchronous path uses, via a new `Daemon::waker`.
* **`lambo_stats` gains ten unconditional keys.** The difference from the
  `ledger_*` keys' gating is not an inconsistency: the ledger is an optional
  subsystem, so "off means byte-identical" is a promise that can be kept for it;
  the write queue has no off switch, so there is no baseline payload left to
  preserve. `write_queue_bound` never appears without `write_queue_measured`, or
  the unmeasured floor would read as a measurement, and
  `write_queue_items_per_sec` reports the **raw** rate even when the bound was
  clamped so the two cases can be told apart.

#### The coupled residual J2 handed over, discharged

J2-R2-7 / J2-R3-3 described the pump's uncapped `inflight` list and its un-raced
write burst as one item, and asked J3 to bound both. **The mechanism is receipt
*waiting*, and the correction belongs in the record:** a non-waiting ack returns
immediately and never enters the pump's `inflight` list, so the queue bound is
not the burst length. A *waiting* `lambo_stats` call is what occupies an
in-flight slot for its whole duration, and `answer_lost` writes one un-raced
frame per slot. So both ends of the waiting surface are bounded —
`RECEIPT_WAIT_MAX` (4 s) caps how long one wait holds a slot,
`MAX_CONCURRENT_RECEIPT_WAITS` (16) caps how many exist — and the link is a
build guard, `const _: () = assert!(MAX_CONCURRENT_RECEIPT_WAITS * 2 <=
INFLIGHT_DEPTH_WARN)`, which is why `INFLIGHT_DEPTH_WARN` became `pub(crate)`.
Half the warn threshold is left for ordinary traffic, so receipt waits alone
cannot be what trips it. Neither ceiling can now move without the other being
considered.

#### The `ledger_queued_lines` arithmetic, re-derived

The queue keeps its **own** counters and never touches `LedgerCounters`, so
`accepted − written − write_failed` keeps its exclusivity argument intact: no
new class enters the ledger's `accepted`. The queue mirrors the discipline
deliberately — a queue-full or byte-cap reject never enters the queue's
`accepted`, `outstanding = accepted − applied − failed` is one expression
serving both the live gauge and the shutdown count, and `abandoned` is a **label
on a subset of `failed`**, not a fourth term. Pinned by
`outstanding_excludes_refusals_because_they_never_reached_accepted`, which
asserts both wrong formulas wrong — and the naive one *panics* on
subtract-with-overflow in a debug build, which is why `outstanding()` is
saturating rather than relying on the invariant.

#### A declared regression: I1's metric-2 facts moved to the receipt

`created`, `matched`, `semantic_merged`, `reinforced` and `edges` cannot ride an
ack issued before the write. They are **relocated, not dropped**: the receipt
carries all five plus true `created_count` / `matched_count` beside the
id lists (truncated at 64), and the ledger line carries `concepts_requested`,
`admitted` and `receipt` so the two join. The cost is real and named:
`scripts/observability/dedup_rate.py` and `duplicates.py` read those keys off
the line, so for **MCP-driven sessions** they now see no derive facts. CLI-driven
sessions are unaffected (they use the synchronous path), and `duplicates.py`'s
store-side half reads the graph, so its cross-check still works.

**Handed forward, not fixed here:** the repair is a ledger line for the write's
*completion*, carrying the same fact keys. That is a ledger **schema** change —
it moves `_ledger.py`, `dedup_rate.py`, `duplicates.py`, the observability
README and `verify.sh` — and doing it inside J3 would have meant two append
paths for one tool, which is precisely the drift hazard I-round3's flip D warns
about. It belongs with whoever next owns ledger artifacts. The README says all
of this at the fact table, so nobody reads a zero as a zero.

#### Two defects found by measuring the binary, not by a test

Both are worth recording because a test suite that was fully green did not see
either.

1. **`lambo_stats` with a `wait_ms` reported the session as it was before the
   wait.** The payload was snapshotted first and the receipt resolved after, so
   a call that blocked for a write returned `write_queue_applied: 0` and
   `concept_count: 0` *beside a receipt in the same payload saying `applied`
   with a created node id*. A payload that contradicts itself is worse than a
   slow one. Fixed by resolving the receipt first; pinned by
   `a_waiting_stats_call_reports_the_session_after_the_wait`.
2. **The queue clamp was a false stated reason.** `PROBE_MAX_CREDIBLE_RPS = 128`
   claimed to mark where a probe stops being credible, reasoning from
   §Measurements' 4-wide recall figure (4 / 64 ms ≈ 62 items/s) that twice that
   was implausible. Probing this machine's own llama.cpp BGE-M3 measured
   **110–141 items/s** — above the "implausible" ceiling — so the clamp would
   have decided the bound on an ordinary local embedder while claiming to guard
   against stubs, and "a ceiling measured on the deployment's own embedder"
   would have been decorative on exactly the deployment it was written for.
   Re-derived from a property of the module instead: every outstanding job holds
   a `Pending` receipt and eviction is oldest-first, so a queue deeper than the
   receipt store could evict the receipt of a *running* write and answer
   `expired` about it. Hence `WRITE_QUEUE_MAX = MAX_RETAINED_RECEIPTS / 4`
   (1024), with `MAX_RETAINED_RECEIPTS` raised 1024 → 4096 (~10 MiB at the
   door's worst case) because at 1024 the derived clamp was still under the
   measured rate. `MEASURED_LOCAL_EMBEDDER_RPS = 141` is now a constant and
   `PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS` is a build guard — the
   guard the first version failed.

A third, found while wiring: the first version ran `graph::derive::validate` on
the call path for every strategy, but hybrid's pre-pass is a *different* set of
rules (it omits the repeated-`Observation` and single-`Hierarchical`-parent
rejections), so a concurrent re-derive began failing at ack with a `store error`
on writes hybrid had always accepted. Validation that disagrees with the write is
worse than none; the pre-pass is now chosen by `match_strategy`.

And one in the new test rig itself, recorded because it cost a 120 s watchdog:
`*m.lock() = *m.lock() + d` on a `parking_lot::Mutex` keeps the right-hand guard
alive across the left-hand acquire and self-deadlocks.

#### Constants, and where each number comes from

| Constant | Value | Derived from |
| --- | --- | --- |
| `WRITE_QUEUE_DRAIN_BUDGET` | 2 s | A quarter of `CLOSE_FLUSH_GRACE` (8 s), *carved out of* it rather than added, so the quiesce cannot be why a `close` misses the window `serve` gives it. One constant serves both admission projection and quiesce, so a queue cannot admit more than shutdown will wait for. Build-guarded. |
| `WRITE_QUEUE_MIN` | 4 | `PROBE_CONCURRENCY`, and defined from it: the floor must not drop work a 4-wide measurement has shown the deployment absorbs. |
| `WRITE_QUEUE_MAX` | 1024 | `MAX_RETAINED_RECEIPTS / 4` — see defect 2. Build-guarded. |
| `PROBE_CLAMP_RPS` | 512 | Derived: `WRITE_QUEUE_MAX / WRITE_QUEUE_DRAIN_BUDGET`. Build-guarded above 3× the measured local embedder. |
| `WRITE_QUEUE_MAX_BYTES` | 16 MiB | `MAX_CONTENT_BYTES × 1024` — a thousand maximal strings. A count is the wrong unit for memory: at the door's caps one maximal `derive` retains ≈ 9 MiB, so this admits one whole and refuses a second. |
| `PROBE_CONCURRENCY` | 4 | §Measurements' parallelism figure is a 4-wide one; the probe re-measures the rate per deployment, this fixes only the width. |
| `PROBE_BUDGET` | 5 s | The worst an admission can wait, since admission blocks on the probe rather than falling back to a constant. Generous because a cold llama.cpp first token takes seconds and calling that "unmeasurable" would floor a warm deployment for life. |
| `RECEIPT_RETENTION` | 300 s | Above the **227 s** worst `flush_lag` in §Measurements — the applied-but-not-durable window a receipt has to outlive, or the widened crash window is unauditable from the surface that describes it. Build-guarded above `HYBRID_IO_TIMEOUT + WRITE_QUEUE_DRAIN_BUDGET`, so `expired` is unreachable for a running job. |
| `MAX_RETAINED_RECEIPTS` | 4096 | A ~10 MiB budget at the door's worst case (2.4 KiB per receipt: a summary plus 64 × 36-byte ids). The time bound alone cannot do this job — 300 s against `DEFAULT_RATE_LIMIT_RPS` (50/s) is 15 000 receipts. |
| `MAX_RECEIPT_IDS` | 64 | `MAX_CONCEPTS_PER_DERIVE`, and defined from it. |
| `RECEIPT_WAIT_MAX` | 4 s | Two drain budgets: a job admitted when the queue was full is projected to *start* at one, so the second is its own service time plus slack. Build-guarded, because a wait shorter than the admission promise would time out on the very jobs it exists for. |
| `MAX_CONCURRENT_RECEIPT_WAITS` | 16 | Half of `INFLIGHT_DEPTH_WARN` (64) left for ordinary traffic. Build-guarded against it — the J2-R2-7 / J2-R3-3 link. |
| `MAX_PIGGYBACK_RECEIPTS` | 8 | One screen of one-line notes; the rest stay queued and the note says how many. |

#### What J3 did not change

No new transport. No change to `lambo_reserve`'s synchrony — its result *is* the
caller's next action. No MCP notifications: a notification lands in a client log
rather than the model's context, which is the failure this workstream exists to
fix. No change to the lease, the fencing token, or the proxy's forwarding — the
receipt rides *inside* tool responses, so the byte pipe forwards it untouched,
and `src/mcp/proxy.rs` has no J3 change at all beyond one `const` becoming
`pub(crate)` for a build guard. Dedup is unaffected: embedding still precedes
insertion. Not J4's ledger lines (see the declared regression), not J5's docs
beyond the two `mcp.mdx` mirrors this change's own surface required.

## J4 — Lease conflicts leave an artifact

A serve that loses the lease exits before it can open a ledger, so the most common
multi-agent failure is structurally invisible to I1 as specified. Two halves: a **pre-lease
startup line** written before the acquire attempt, and the **holder recording refused
takeovers**. Without these, metric 6 friction and every "why did this agent have no memory"
question stay unanswerable from artifacts.

J2 makes it cheaper, since a proxying serve is alive and can write its own lines.

## J5 — Transport defaults and config layering

* Document HTTP as the default for any machine running more than one client. DOGFOOD.md's
  option (a) reasoned that one orchestrator holds one connection and subagents inherit it;
  that holds for subagents, not across sessions, which the client app multiplies by
  restoring prior ones at launch.
* **A transport migration touches every config layer on the machine.** Repointing the
  project `.mcp.json` while pi's user-scope `~/.pi/agent/mcp.json` still carried a
  `command` entry produced a server holding both `command` and `url`, which pi rejected
  even though its probe of the endpoint succeeded.
* Consider `lambo serve --print-client-config <client>` so migration is a copy rather than
  a hand-edit per layer.

Lowest priority: once J2 lands, transport stops being the user's problem to get right.
Worth keeping as hygiene.

**Catch:** the `--ledger`/transport prose lives in four hand-maintained mirrors —
`docs/reference/{cli,mcp}.mdx` and `site/src/content/docs/{cli,mcp}.mdx` — kept as
byte-identical pairs with **no drift gate** (`verify.sh` drift-checks only the
observability sample). J5 edits all four; add the one-line `diff` of each pair to CI
*before* editing them, so the edit lands against a gate instead of installing the first
drift.

---

## Measurements (2026-08-19, pinned `3039b82`, SQLite + BGE-M3 q8_0 on CPU)

| | measured |
| --- | --- |
| Local MCP round trip, no work (the proxy hop) | 0.31 to 0.48 ms |
| `lambo_recall` | 27 to 123 ms |
| `lambo_derive`, warm | 27 ms |
| Raw embedding call, same text | 22 to 27 ms |
| 4 recalls sequential vs concurrent | 380 ms vs 64 ms (5.94x) |
| 10 embeddings batched vs one-at-a-time | 234 ms vs 198 ms (no win) |
| `flush_lag` observed | 145 to 227 s |
| `LEASE_TTL` / heartbeat | 45 s / 15 s |

The proxy hop is under 1% of any call that embeds, and roughly 100x smaller than a single
recall's run-to-run jitter. The hub parallelises, so it is not a serialization point.
Configured ceilings (`rate_limit_rps=50`, `max_sessions=32`) bite long before the hop does.
Every figure is one rig, not a property of lambo.

## What J does not change

* **The lease stays.** No weakening, no preemption, no change to the fencing token. A proxy
  takes no lease and presents no token; every durable write still happens in the holder,
  under the holder's token. The proxy moves the call, not the write.
* **Single-writer stays the deployment model** (spec §2.2). J is about what the losers do.
* **Not a substitute for I.** J4 is a requirement placed on I1, not a second ledger.

## Done when

- [x] A client whose `agent_id` differs from the serve's can take and release a soft lock,
      and two clients through one hub hold distinct locks (J1) — through one serve process;
      the proxy sense of "hub" is J2's
- [x] `lambo serve` against a held session starts as a proxy instead of exiting 1, and every
      tool call including writes succeeds through it (J2) — `--transport stdio`;
      `--transport http` still exits 1 by design, see the status note
- [x] A write through a proxy is durable in the holder and visible to that client's next
      recall, pinning read-your-writes across the hop (J2) — and to the *hub's* client too,
      which is the "one graph, not N" half
- [x] `session_leases` carries the holder's endpoint and `serve` binds a local socket even
      under `--transport stdio` (J2)
- [~] Killing the holder uncleanly leaves proxies failing honestly rather than hanging, and
      a new holder is electable within one `LEASE_TTL` (J2) — the honest-failure half is
      pinned with a bounded wait, so a hang fails the test; "electable" holds literally (the
      lease lapses and the next serve *start* wins it, including `resolve_role`'s in-process
      startup election) but NOT in the strong sense of a running proxy electing itself
      mid-session, which the wedge invariant forbids until promotion exists. **Excluded
      by design, stated (J2-R2-2):** a client starting *promptly* after an abrupt holder
      death is refused within that start, with an actionable retry interval — not recovered.
      Any abrupt death leaves 30–45s of lease and the client-tolerance election budget is
      20s, so the arithmetic cannot be waited out; "electable within one `LEASE_TTL`" means
      the start *after* the lease lapses, and the refusal names when that is
- [x] Two clients on one machine, both wired over stdio, both fully working — verified with
      two different client products, not two sessions of one (J2). **Verified live,
      2026-08-20, twice**: an agent-run probe with `cursor-agent 2026.08.11` +
      `opencode 1.18.18` against the implementation build (found J2-L1/J2-L2), and the
      round-2 review's re-run against the remediated build — proxying, cross-product
      read-your-writes both directions, on unmodified default wiring even with divergent
      endpoint directories (`adve-review-mooshik-J2-round2.md`). The committed test still
      drives two subprocesses of one binary — the two-product claim lives in those live
      runs and in DOGFOOD-FINDINGS, not inside `cargo test`, and re-verifying it after a
      re-pin is a runbook act (the probe harness is reusable). pi was unusable (no ready
      generative provider) and is NOT covered by this box
- [x] `lambo_derive` returns after validation without waiting on the embedder, and its call
      time drops to the round-trip floor (J3) — **measured at the release binary over real
      stdio against the rig's live BGE-M3 on CPU: 14.111 ms median before, 0.048 ms median
      after**, which is at or below the same binary's ack against an embedder that does no
      work at all (0.076 ms). See §J3 Status for the full table and why those absolute
      figures differ from §Measurements' 27 ms. Pinned in `cargo test` as a *property*
      rather than a stopwatch (`the_ack_lands_before_the_embedder_is_called`: the ack
      returns with the write parked in a gated embedder), because a timing assertion in CI
      would be flaky
- [x] Every write ack carries a receipt; outcomes are retrievable by it; expired and
      restart-lost answer distinctly, never "unknown" (J3) — seven states, `expired` /
      `restart_lost` / `never_issued` all distinct, and `forbidden` for another agent's
      receipt (per-agent scoping, J1). Retrieval is `lambo_stats(receipt=…)`, **not an
      eighth tool** — deviation argued in §J3 Status. `restart_lost`'s wording is
      word-for-word consistent with the proxy's -32002
- [x] Waiting on a receipt restores read-your-writes for a caller that asks (J3) —
      `lambo_stats(receipt=…, wait_ms=…)`, clamped to `RECEIPT_WAIT_MAX` rather than
      refused, and exercised end to end through a **proxy** as well as in-process. A
      timed-out wait answers `pending`, which is honest rather than a failure
- [~] The queue bound comes from a ceiling measured on the deployment's own embedder, drops
      are counted in `lambo_stats`, and a burst degrades visibly (J3) — measured by a 4-wide
      concurrent probe of the deployment's own embedder, spawned at build so it costs
      startup nothing, and admission *awaits* it rather than falling back to a constant, so
      there is no constant-bounded window. Verified at the binary: the live BGE-M3 measured
      131.9 items/s and got bound 264, **not** clamped. Drops are `write_queue_dropped`
      beside nine other keys and each drop says so on its own receipt.
      **Tilde, and here is the honest limit:** a probe *failure* falls back to
      `WRITE_QUEUE_MIN` and reports `write_queue_measured: false`, so a deployment whose
      embedder is down at startup runs on a floor nothing measured until it restarts — the
      probe is one-shot, not retried. And an embedder above `PROBE_CLAMP_RPS` (512 items/s)
      is clamped by receipt retention rather than by its own throughput; that is by design
      and `write_queue_items_per_sec` still shows the raw rate, but on such a deployment the
      bound is not the embedder's ceiling. Neither case is a burst that degrades invisibly,
      which is what the box is for; both are reasons not to tick it flat
- [x] One agent's writes apply in submission order, pinning the `Temporal` chain (J3) — and
      with two agents interleaving through one process, the §13 conflict sentence's `writer`
      is **measured** rather than assumed: J1 made the same-instant collision path
      non-degenerate (J1-R1-8). Satisfied as the amendment requires, by filtering the
      session-wide chain on `agent_id`
      (`interleaved_agents_each_keep_their_own_order_on_the_temporal_chain`, which also
      asserts the chain *actually* interleaves or the filter would prove nothing). Stronger
      than specified in one respect and it is worth stating: the interaction is opened on
      the call path, so chain order is submission order **by construction** and cannot be
      corrupted by an out-of-order drain at all. Per-agent FIFO lanes are still enforced in
      the drain, for the separate reason that insertion order decides which of two identical
      concepts is `created` and which is `matched`
      (`each_agents_writes_drain_in_that_agents_submission_order`)
- [ ] A refused lease acquisition appears in the ledger from both sides (J4)
- [ ] Docs state the multi-client default and the every-layer config rule (J5)
- [x] The concurrent-client probe from 2026-08-19 is a committed test, not a shell transcript
      (J2) — `tests/serve_proxy_multi_client.rs`

---

## Rig re-pin rides with J's landing

When J lands, the dogfood **rig re-pin and the runbook update are one act, not two**:
J changes `serve`'s startup behaviour (J2's proxy-on-refusal, J5's transport defaults) in
ways that make parts of [DOGFOOD-SETUP.md](DOGFOOD-SETUP.md) §4/§5 stale the moment a
J-carrying binary serves — §5's "one client at a time" interim rule is *specifically*
written to be deleted by J2, and the per-client stdio blocks change meaning when a losing
serve proxies instead of exiting. Re-pinning without the runbook edit ships a rig whose
operating instructions describe the previous binary; editing the runbook without the
re-pin describes a binary that is not running. Do both in the same commit/upgrade event,
and let the heartbeat's `git_sha` change be the proof, per §2.

Status note: this machine's rig was re-pinned to `0f672f1` (the I-close, ledger-carrying
binary) on 2026-08-20; other machines re-pin per the runbook whenever they next set up —
nothing else re-pins tonight.

### What J2 and J3 make stale in DOGFOOD-SETUP.md — recorded, not yet edited

J2 did **not** touch the runbook, on purpose: the re-pin and the runbook edit are one act
(above), and editing it now would describe a binary no machine is running. This is the list
that edit has to work through, written while the changes were fresh.

* **§5 "The one-writer reality" shrinks to a pointer**, exactly as its own last paragraph
  predicts — but only for `--transport stdio`. Both interim rules can go for stdio: "one
  client registered at a time" is no longer needed, and "more than one client ⇒ HTTP" stops
  being a requirement (it stays a legitimate *choice*, and J5 still owns the default). The
  paragraph describing the losers as exiting 1 with no error reaching the agent becomes
  **history** and should be marked as such rather than deleted — it is the defect the
  workstream exists for.
* **§4 "Client wiring" changes meaning without changing text.** The per-client stdio blocks
  are now correct as written: the first serve to start becomes the hub and later ones proxy.
  What the section should gain is one sentence saying *which* process holds the lease is
  whichever started first, and that this is invisible to the client — because an operator
  debugging "why is one client slow to start" needs to know a losing serve may wait for a
  dead holder's lease to lapse (up to `ELECTION_BUDGET`, 20s) before it either proxies, takes
  over, or refuses with the remaining lease time named.
* **§2 "The pinned binary"** — the endpoint's socket path is derived from the session **and
  the store identity**, so two machines with different store paths do not collide, but two
  *different binaries* on one machine serving one session will refuse to proxy to each other
  if their endpoint schemes ever diverge. That makes "binaries do not travel" slightly more
  load-bearing than it was, and the re-pin should be all-at-once on a machine.
* **§6 "Smoke test"** gains a cheap and worthwhile check: after wiring two clients, confirm
  the lease row names one holder and carries an `endpoint`, and that the socket exists. That
  is the one-line proof the hub is real rather than assumed. **Add one more, from the
  round-1 review (J2-R1-3/J2-R1-19):** confirm the *directory* holding it is 0700 and owned
  by you — `$XDG_RUNTIME_DIR/lambo`, else `/tmp/lambo-<uid>`.
  "The socket exists" is already on this list; "the directory is 0700 and yours" is the
  check that actually fails on a shared box, and `lambo serve` refuses to bind rather than
  degrade when it is not. The user-facing half of this now lives in `mcp.mdx`, so the
  runbook can point at it instead of restating it.
* **New, and worth its own line:** the operator-visible artifact of a proxying serve is a
  stderr line, `lambo serve: proxying to the session holder`. Nothing reaches the ledger
  (J4's), so "which of my serves is the hub" is answered from the lease row or from that
  line, and the runbook should say which.
* **J3, §7's agent protocol.** The instruction the runbook hands agents — derive
  decisions-with-why, then recall — now needs one clause: `lambo_derive` and
  `lambo_record_action` return before the write is applied, so an agent that must
  *read back* what it just wrote calls `lambo_stats` with the ack's receipt and a
  `wait_ms` first. Nothing else about the protocol changes, and the common case
  (write, keep working, read the outcome off the next response's `write receipts:`
  block) needs no instruction at all. The MCP server's own `instructions` string
  already says this to every model that connects; the runbook line is for the
  operator reading a transcript and wondering why a recall came back empty.
* **J3, §2's startup line.** A J3-carrying serve logs one more INFO at startup,
  `write queue: bound measured on this deployment's embedder`, with the measured
  `items_per_sec` and the bound. That line is the fastest way to tell whether the
  rig's llama-server was reachable at startup — a `WARN` in its place means the
  queue is on its unmeasured floor — so it belongs in §6's smoke test beside the
  lease-row check.
