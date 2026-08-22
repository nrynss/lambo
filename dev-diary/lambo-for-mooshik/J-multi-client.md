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
* `blast_radius.py`'s "By agent (who was warned)" attributes to the caller and becomes
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

**Status: implemented; three review rounds remediated; REDESIGNED at round 3
(durable intents) — see [§J3 Status](#j3-status--landed) below** for what
shipped, the measured latency, the deviations argued, every constant's
derivation, and the per-round findings, ending with
[§Round-3 and the durable-intent redesign](#round-3-and-the-durable-intent-redesign-adopted--2-p1-1-p2-3-p3-all-closed),
which is where the current design lives. The spec below is the spec the first
implementation was built against; the redesign's design of record is
`J3-durability-redesign.md` beside this file.

A warm `lambo_derive` is 27ms, of which 22 to 27ms is the embedding call (this
line's earlier "22 to 25ms" was a misquote of §Measurements — J3-R3-5). Durability is
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

**Status: implemented on `wt/j3`; rounds 1–3 of the review remediated; the
round-3 remediation is a REDESIGN (durable post-validation intents), adopted.**
Four staged commits (`427fabf` pipeline, `dcf29de` MCP surface, `f9abfbb` tests,
`3e1bea4` two defects found by measuring the binary), plus this note — three more
for the round-1 review's thirteen findings (see
[§Round-1 remediation](#round-1-remediation-1-p1-4-p2-8-p3--all-closed)), whose
P1 changed how the queue bound is derived, three more for round 2's nine (see
[§Round-2 remediation](#round-2-remediation-1-p1-3-p2-5-p3--all-closed)), whose
P1 changed what the bound is allowed to rest on, and five more for round 3's six
(see [§Round-3 and the durable-intent redesign](#round-3-and-the-durable-intent-redesign-adopted--2-p1-1-p2-3-p3-all-closed)),
whose two P1s ended the estimator's career entirely. Every round's P1 was the
same hazard — an acked write abandoned at a clean `close()` — and every one was
found by measuring the release binary rather than by reading the code. **The
subsections between here and the round-3 one are the history of the estimator
design and are kept as history**: where a sentence below describes admission
bounds derived from measured rates, probe-era ceilings, or a close that
abandons what it cannot drain, the round-3 section is the current truth.

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
  **Scoped, at round 1, to one agent's *sequential* submissions** (J3-R1-10):
  the chain position is pinned by `begin_interaction_as` and the lane position by
  the `lanes.lock()` inside `admit`, two critical sections with no ordering
  between them across threads, so two `lambo_derive` calls one agent has in
  flight *simultaneously* can be chained in one order and drained in the other.
  The consequence is confined to created/matched attribution between those two
  calls, and a caller that fires two writes at once has asserted no order for
  them to keep. Closing the window would mean opening the interaction under the
  lane lock, nesting the graph write lock inside it — not worth it for a
  guarantee nobody can use. The tool instructions and both `mcp.mdx` mirrors now
  say "writes you send one after another are applied in that order (two you fire
  at once have no order to keep)" rather than the unscoped claim.
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
  past the budget is aborted **and joined**, because aborting alone proves
  nothing (the R3-1 lesson). *What happens to the remainder changed at round 3*:
  it used to be settled `failed` with a session-closed reason; under the
  durable-intent redesign it settles `intent_durable` — the write survives as a
  durable intent the next serve applies — and is counted in
  `write_queue_deferred`, never in `abandoned`.
* **The daemon's wake is unchanged**: a background write pokes it through the
  same `Notify` the synchronous path uses, via a new `Daemon::waker`.
* **`lambo_stats` gains eighteen unconditional keys** (ten at first landing; round 1 added `write_queue_lane_bound`, `write_queue_bound_source`, `write_queue_serial_items_per_sec` and `write_queue_dropped_closed`; round 2 added `write_queue_probe_serial_items_per_sec`; round 3 added `write_queue_deferred` and `write_queue_replayed`; the round-1 review remediation added `write_queue_replay_owed`). The keys added since first landing are the ones that need explaining, and J3-R2-8 was right that the bullet did not explain them: **`write_queue_lane_bound`** is the bound that actually refuses one agent's burst, reported beside the aggregate `write_queue_bound` — since round 3 both are static caps (the per-agent fair share and the memory ceiling), not measurements; **`write_queue_bound_source`** says which evidence the *rate telemetry* rests on (`probe`, `observed`, `unmeasured`), because "measured" alone cannot distinguish a startup estimate from this deployment's own timed writes and the two differ by 4× on ordinary content; **`write_queue_serial_items_per_sec`** is the 1-wide rate the lane drains at — telemetry since round 3, never a bound input; **`write_queue_probe_serial_items_per_sec`** keeps the probe's figure beside whichever rate is in force, because the *gap* between them is the diagnosis (J3-R2-4); **`write_queue_dropped_closed`** separates a refused shutdown tail from real backpressure; **`write_queue_deferred`** counts acked writes a clean close handed to the next serve as durable intents (a fourth settle class — neither applied, failed, nor lost); **`write_queue_replayed`** counts a previous process's intents this session applied at attach, deliberately not summed into `applied` so `outstanding = accepted − applied − failed − deferred` stays exact; and **`write_queue_replay_owed`** is the replay DEBT rather than a total — intents this session found owed and has not yet paid — which is what makes "the embedder was down at attach, so nothing was consumed" a number an operator can read instead of an absence they have to infer (J3 round-1 N1). The difference from the
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
`accepted`, `outstanding = accepted − applied − failed − deferred` is one
expression serving both the live gauge and the shutdown count, and `abandoned` is
a **label on a subset of `failed`**, not a fourth term — while `deferred` *is*
one, because a close-deferred job settled `intent_durable` left this process's
custody without being applied or failed. (Round 1's N5 caught both this sentence
and its `writeq.rs` twin still writing the three-term form after round 3 added
the fourth: a section whose thesis is "one expression, and it cannot drift
between them" had drifted in the two prose copies while the code was right.
Neither copy is the authority; `WriteQueueCounters::outstanding` is.) Pinned by
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
   (1024), with `MAX_RETAINED_RECEIPTS` raised 1024 → 4096 because at 1024 the
   derived clamp was still under the measured rate. (The "~10 MiB at the door's
   worst case" this paragraph originally quoted was wrong — see J3-R1-6 in the
   constants table: ≈31 MiB. It was the *stated reason* for the raise, so the
   correction matters, and the raise still stands on the clamp guard alone.)
   `MEASURED_LOCAL_EMBEDDER_RPS = 141` is now a constant and
   `PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS` is a build guard — the
   guard the first version failed. **Round 1 replaced the eviction half of this
   argument** with structure rather than arithmetic (J3-R1-3): an unsettled
   receipt is skipped by both the expiry sweep and the eviction scan, because
   refusals get receipts too and a drop storm could otherwise push a parked
   job's `Pending` entry out of the newest quarter.

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

**This table is the round-2 record, kept as history.** The round-3 redesign
deleted `DRAIN_PROJECTION_SHARE`, `WRITE_QUEUE_MIN`, `WRITE_QUEUE_LANE_MIN`,
`PROBE_LANE_CEILING` and `PROBE_AGGREGATE_CEILING` (with the argument recorded
at the site in `src/writeq.rs` that held them), re-derived `PROBE_CLAMP_RPS` as
a pure telemetry clamp (numerically unchanged), retired `PROBE_BUDGET`'s
"admission blocks on the probe" role (admission is instant; nothing awaits the
probe), and added `WRITE_QUEUE_LANE_MAX = WRITE_QUEUE_MAX /
MAX_CONCURRENT_RECEIPT_WAITS = 64` — the per-agent fair share, a division of
two structural constants rather than a projection of any rate. See the round-3
section's constants table for the current derivations.

| Constant | Value | Derived from |
| --- | --- | --- |
| `WRITE_QUEUE_DRAIN_BUDGET` | 2 s | A quarter of `CLOSE_FLUSH_GRACE` (8 s), *carved out of* it rather than added, so the quiesce cannot be why a `close` misses the window `serve` gives it. One constant serves both admission projection and quiesce — **half the property; the other half is that the rate must be the drain's own, which is what J3-R1-1 was.** Build-guarded, and now also guarded above zero seconds (`PROBE_CLAMP_RPS` divides by `as_secs()`). |
| `DRAIN_PROJECTION_SHARE` | 2 | **New at round 1.** The share of the drain budget a bound may project against: half, because a lane filled to exactly `rate × budget` leaves no slack for the part of a job the rate does not cover. The probe times the embedder only, and §Measurements puts the embed at **22 to 27 ms** of a 27 ms warm derive — 0 to 5 ms of remainder on that rig. **Round 2 measured the general case and it is much less generous** (J3-R2-8): at the release binary with 512-byte concepts the embed is 36.3 ms and the whole of `run` is ~64 ms, so the remainder is ~78% of the embed, not ~20%. Half the budget still covers it, with far less margin than the old "~1/5" implied — and the docstring's "22-25 ms" was a misquote of §Measurements inside a load-bearing derivation, now corrected. One share for both sources, so there is one rule: *a lane may hold what the drain retires in half the budget.* |
| `WRITE_QUEUE_MIN` | 4 | `PROBE_CONCURRENCY`, and defined from it: the **aggregate** floor must not drop work a 4-wide measurement has shown the deployment absorbs. |
| `WRITE_QUEUE_LANE_MIN` | 1 | **New at round 1.** Floor on the *per-lane* bound. Not `WRITE_QUEUE_MIN`, because nothing has been demonstrated about one lane's depth; one, because a lane bound of zero is an outage rather than backpressure. This is the floor of the one declared hole in the close-drain invariant: a single write slower than the whole drain budget cannot be made to finish inside it, so it is abandoned with an honest receipt rather than refused. **At round 2 the hole's width became `PROBE_LANE_CEILING` rather than this** — see limit (3) of the Done-when box. |
| `WRITE_QUEUE_MAX` | 1024 | `MAX_RETAINED_RECEIPTS / 4` — see defect 2. Build-guarded, and the guard **replaced** at round 1: `WRITE_QUEUE_MAX * 4 <= MAX_RETAINED_RECEIPTS` is algebraically vacuous under integer division (J3-R1-7), so it now asserts `WRITE_QUEUE_MAX >= WRITE_QUEUE_MIN && WRITE_QUEUE_LANE_MIN <= WRITE_QUEUE_MAX`, which can fail. **Round 2 narrowed who may reach it** (J3-R2-1): only an `observed` calibration, the one source that sampled the workload. |
| `PROBE_CLAMP_RPS` | 1024 | Derived: `WRITE_QUEUE_MAX × DRAIN_PROJECTION_SHARE / WRITE_QUEUE_DRAIN_BUDGET`. Was 512 before the projection share existed. Build-guarded above 3× the measured local embedder, now with more room. |
| `WRITE_QUEUE_MAX_BYTES` | 16 MiB | `MAX_CONTENT_BYTES × 1024` — a thousand maximal strings. A count is the wrong unit for memory: at the door's caps one maximal `derive` retains ≈ 9 MiB, so this admits one whole and refuses a second. |
| `PROBE_CONCURRENCY` | 4 | §Measurements' parallelism figure is a 4-wide one; the probe re-measures the rate per deployment, this fixes only the width of its **concurrent** leg. It sizes the aggregate bound only — the per-lane bound comes from the serial leg. |
| `PROBE_WARMUP_EMBEDS` | 1 | **New at round 1 (J3-R1-2).** Embeds thrown away before the probe starts timing, so the model-load cost is paid out of the probe's budget rather than out of the measurement. |
| `PROBE_EMBEDS` | 7 | Derived: `PROBE_WARMUP_EMBEDS + 2 + PROBE_CONCURRENCY` — the warm-up, **two** serial legs (J3-R2-1: one at `PROBE_TEXT`, one at `PROBE_TEXT_BYTES`), the concurrent leg. The seventh is best-effort, so this is a budget input rather than a requirement. |
| `PROBE_TEXT_BYTES` | 1024 | **New at round 2 (J3-R2-1).** Size of the probe's representative serial leg. Bounded from below by the workload — lambo's own dogfood concepts run 700 to 1500 bytes — and from above by the embedder: this rig's llama-server answers 1280 B in 75.8 ms and returns **HTTP 500 on 8 of 8 calls at 1536 B**, so a representative leg takes the largest power of two under the smallest refusal measured here. The text is `PROBE_TEXT` repeated to that length, so the pair of legs differs in length and nothing else. |
| `PROBE_LANE_CEILING` | 4 | **New at round 2, and the actual fix for J3-R2-1.** Ceiling on the per-lane bound while the rate is the probe's. `OBSERVED_MIN_SAMPLES`, and defined from it: *admit no more on the probe's word than it takes to replace the probe's word.* The population exposed to a startup estimate is exactly the population that retires it. |
| `PROBE_AGGREGATE_CEILING` | 16 | **New at round 2 (J3-R2-1).** `PROBE_CONCURRENCY × PROBE_LANE_CEILING` — the widest the concurrent leg measured, each lane holding the exposure window the lane ceiling authorises. The aggregate leg has the same defect the serial one did, so it gets the same treatment rather than an argument about why it is different. |
| `OBSERVED_MIN_SAMPLES` | 4 | **New at round 1 (J3-R1-2).** `PROBE_CONCURRENCY`, so the observed rate never rests on fewer embeds than the probe's own leg did. Past it, real write service times replace the probe's serial figure. |
| `OBSERVED_EWMA_WEIGHT` | 4 | `PROBE_CONCURRENCY` again, as a divisor: a new sample gets 1/4 weight, so the average moves most of the way in about one probe's width of samples. A weight of 1 would track a single slow write; a much larger one would keep a warm figure after the embedder degraded. |
| `PROBE_BUDGET` | 5 s | The worst an admission can wait, since admission blocks on the probe rather than falling back to a constant. **Unchanged even though the probe now takes seven embeds rather than four, one of them at `PROBE_TEXT_BYTES`**, deliberately: raising it raises the worst ack latency, and a deployment too cold to answer seven embeds in 5 s is better served starting on the floor and being corrected by observation than believing a number taken while its model was loading. Round 2 makes the trade *cheaper*, not dearer — with `PROBE_LANE_CEILING` in force, `unmeasured` and `probe` differ by a lane bound of one against four. Measured warm at the binary, all seven embeds land in ~180 ms of the 5 s; and the optional representative leg is separately bounded by half the remaining budget, so a *hang* there cannot starve the required concurrent leg. |
| `RECEIPT_RETENTION` | 300 s | Above `MEASURED_WORST_FLUSH_LAG_SECS` — the applied-but-not-durable window a receipt has to outlive, or the widened crash window is unauditable from the surface that describes it. **Measured from the settle, not the issue** (J3-R1-3), which is what makes that the right comparison: the durability lag starts when the write applies. An unsettled receipt never expires and is never evicted. |
| `MEASURED_WORST_FLUSH_LAG_SECS` | 227 | **New at round 1.** The worst `flush_lag` in §Measurements, as a constant so the retention relation can be a build guard — the same idiom as `MEASURED_LOCAL_EMBEDDER_RPS`. It **replaces a false guard**: `RETENTION > HYBRID_IO_TIMEOUT + DRAIN_BUDGET` stated the conclusion "so `expired` is unreachable for a running job" and did not prove it, because expiry keyed on issue time and the drain budget is a projection of queue residency, not a bound on it. |
| `MAX_RETAINED_RECEIPTS` | 4096 | The **corrected** worst case is ≈31 MiB, not ~10 MiB (J3-R1-6): the old figure counted one of `AppliedSummary`'s two id lists, and `2 × MAX_RECEIPT_IDS` = 128 ids at 60 bytes apiece (36 text + a 24-byte `String` header) is ≈8 KiB per receipt. A plain `derive` cannot reach it (`created` + `matched` ≤ `MAX_CONCEPTS_PER_DERIVE`, so ≈16 MiB), but `record_action`'s three resource lists and `derive`'s `parent_of` fan-out can each push `created` past 64. The constant does not move, and the reason is worth stating: 4096 is driven by `PROBE_CLAMP_RPS > 3 × MEASURED_LOCAL_EMBEDDER_RPS` (which needs `WRITE_QUEUE_MAX ≥ 424`, so `MAX_RETAINED_RECEIPTS ≥ 1696`), and the memory figure is that choice's sanity check rather than its source. The time bound alone cannot do this job either — 300 s against `DEFAULT_RATE_LIMIT_RPS` (50/s) is 15 000 receipts. |
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

#### Round-1 remediation (1 P1, 4 P2, 8 P3 — all closed)

`adve-review-mooshik-J3-round1.md` returned REQUEST_CHANGES. The design was not
questioned; the arithmetic between the probe and the drain was. Three commits on
`wt/j3`: the P1 cluster, the two cheap P2s, and this register sweep.

**The P1 cluster (J3-R1-1 P1, J3-R1-2, J3-R1-3) was one root cause — a
projection is not a bound — and was fixed as one change.** Reproduced red first,
at `528ade6`, with the review's own parameters (100 ms/embed, perfectly
parallelising, `Hybrid`, one agent, an 80-deep burst): **58 of 77 acked writes
abandoned at a clean `close()`**, applied 19, quiesce burning its whole 2.0028 s
(the review measured 61 of 80; the difference is how many drained while the burst
was still being submitted). Green on the same test: lane bound 10, accepted 10,
dropped 70, **applied 10, abandoned 0**, quiesce 1.039 s. The invariant now
pinned, per receipt, as a truth table: *every acked write either applies before a
clean close returns, or its receipt says dropped/failed honestly — never silently
abandoned.* The three sides of it are covered by
`one_agents_burst_never_outruns_its_own_lanes_drain_at_a_clean_close` (clean
close), `a_running_jobs_receipt_neither_expires_nor_loses_its_outcome`
(expiry while running), and the three drop-class tests below (refusal at the
door).

The remaining findings, and what each got:

| # | Finding | Disposition |
| --- | --- | --- |
| **J3-R1-1** (P1) | 4-wide projection, 1-wide drain | Per-lane admission from a serial probe leg; `Lanes::lane_outstanding`; both projections against `DRAIN_PROJECTION_SHARE` of the budget; `DropReason::LaneFull` so a receipt says which bound refused it |
| **J3-R1-2** (P2) | The probe measures warmth (7× swing) | Warm-up embed discarded; the lane workers' own service times replace the probe's serial figure after `OBSERVED_MIN_SAMPLES`; `write_queue_bound_source` reports `probe` / `observed` / `unmeasured` |
| **J3-R1-3** (P2) | `expired` reachable for a running job; outcome then discarded | Retention keyed on the **settle**; unsettled receipts skipped by `expire` *and* by `evict` (the count side had the same hole, since refusals get receipts too); `settle_one` settles before it sweeps; the false build guard replaced by a true one |
| **J3-R1-4** (P2) | `derive_async_as`'s docstring claimed one pre-pass for every strategy | Rewritten per strategy, with the five error classes that move under `Hybrid` and why moving them is correct |
| **J3-R1-5** (P2) | The burst test sealed instead of filling; `QueueFull`/`QueueBytes` untested | Renamed to `a_sealed_queue_refuses_and_counts_it`; three new tests for `LaneFull`, `QueueFull` and `QueueBytes` — the last exercising `lanes.bytes` for the first time, including that the byte accounting is a gauge and not a running total |
| **J3-R1-6** (P3) | ~10 MiB counted one of two id lists | Recomputed at the constant and in the table above: ≈31 MiB worst case, ≈16 MiB for a plain derive, and the constant's real driver named |
| **J3-R1-7** (P3) | Vacuous `WRITE_QUEUE_MAX` guard | Replaced with one that can fail, plus a new guard against a sub-second drain budget (the compile-time divide-by-zero the review flagged) |
| **J3-R1-8** (P3) | `write_queue_dropped` conflated backpressure with closing | `dropped_closed` counter and key, summed into `dropped()` so no count vanishes and no never-accepted class is subtracted |
| **J3-R1-9** (P3) | A waiting `lambo_stats` stated its outcome twice | `mark_delivered` takes the just-answered receipt out of the piggyback queue; the proxy test now pins both halves |
| **J3-R1-10** (P3) | "Chain order by construction" has a concurrent-submission window | Scope stated precisely in `writeq`'s §Ordering, `derive_async_as`, the tool instructions and both `mcp.mdx` mirrors, with a test asserting the unscoped sentence is gone |
| **J3-R1-11** (P3) | `dedup_rate.py`'s message named two wrong causes | The async ack is now named first, with `lambo_stats(receipt=…)` and the README's metric-2 note |
| **J3-R1-12** (P3) | No J3-shaped line in the committed sample | **Deviation, argued below** |
| **J3-R1-13** (P3) | Test triples quoted off the lib line | Corrected here; git history not rewritten |

**J3-R1-12 — the deviation, and the argument.** The review asked for two
J3-shaped derive lines and a `record_action` line in `sample/calls.jsonl`. They
went into a **generated fixture inside `verify.sh`** instead, because
`verify.sh` states the sample's design in its own words — *"the committed sample
is deliberately clean-v1: these three cases are generated here instead, so they
cannot perturb the planted facts every check above reads"* — and fact-less lines
in the committed sample would move the exact numbers five checks read (the 66.7%
compliance figure, the "rising" convergence, the per-day rates). A fixture that
perturbs the plants it shares a file with defends one schema by weakening five
checks. The concern behind the finding is fully honoured: the new step plants
`concepts_requested` / `admitted` / `receipt` with no facts, including a
`record_action` and a refused derive, and checks that the report says `n/a`
rather than `0.000`, that all three fact-less calls are counted as such, that the
message names the async ack and the receipt, and — through `--json` — that the
rate is `null` and never a zero. `verify.sh` goes from **40 ok to 46 ok**, all
passing, and `sample/calls.jsonl` is byte-identical (`make_sample.py` untouched).

**J3-R1-13 — the count convention, corrected.** The figures in the four staged
commit messages and in this note (`872/0/1`, `940/0/1`, `550/0/0`) were read off
the `unittests src/lib.rs` line alone. The house convention, which every prior
review in this series used, is the **repo-wide total across all test binaries
including doctests**. Under it, the landing commit `528ade6` was **885/0/3
fixtures, 973/0/3 sqlite, 559/0/0 cockroach** — every profile strictly up from
the parent, with the ignored count unchanged. Nothing was deleted and nothing
de-ignored; the reviewer's own reconciliation proved that name by name. Git
history is not rewritten, so this note is the correction of record. After the
three remediation commits the repo-wide totals are:

| Gate | Result |
| --- | --- |
| `cargo test --all --features fixtures` | **891 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | **979 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | **560 / 0 / 0** |
| `scripts/observability/verify.sh` | **46 ok** |

**The P1, measured at the release binary against the live BGE-M3** — not just
in `cargo test`. Two release builds (`--features store-sqlite,embed-bge`), the
review head `355e85e` and the remediated head, driven over raw stdio JSON-RPC
against the rig's llama.cpp BGE-M3 q8_0 on CPU (`127.0.0.1:8080`). One agent,
200 `lambo_derive` calls back to back, then the pipe is closed so `serve` runs
its **clean** shutdown path, then a fresh `serve` on the same SQLite store counts
what actually landed:

| Run | bound(s) | acked | clean close | concepts in the store after restart | acked writes lost |
| --- | --- | --- | --- | --- | --- |
| `355e85e` #1 | 178 | 178 | 2041 ms (whole budget) | 116 | **62** |
| `355e85e` #2 | 196 | 196 | 2030 ms (whole budget) | 109 | **87** |
| remediated #1 | lane 69 / all 108 | 69 | 1174 ms | 69 | **0** |
| remediated #2 | lane 39 / all 228 | 39 | 616 ms | 39 | **0** |

The review projected "~200 abandoned at every clean close under load" from the
published rates; measured on this rig it is 62 and 87 — the same defect, sized by
how warm the embedder was when the probe ran. The remediated runs show the other
half of the point: the *capacity* still moves with embedder warmth (lane bound 69
one run, 39 the next), and durability no longer does. Everything acked applied,
both times, with the close finishing in a third to two-thirds of its budget.

The ack itself is unchanged where it matters: 20 sequential derives against the
same live embedder ack at a **median 0.074 ms** (min 0.064, max 0.511), all 20
applied, `concept_count: 20`, and a wait on the last receipt returns `applied` —
so read-your-writes on demand still works end to end.

**What flips `write_queue_bound_source`, stated correctly** (J3-R2-5). This note
said the source "read `observed` by the end of that run", and it does not: what
retires the probe's figure is `OBSERVED_MIN_SAMPLES` **completed writes**, not
four *calls*. Twenty acks complete in ~1.2 ms while four writes take ~250 ms, so
a tight 20-call run read immediately after its last ack still reads `probe` —
the reviewer measured exactly that, four times. It does flip, reliably and
one-way, given the writes time to finish; a run that waits on its receipts (the
`lambo_stats(receipt=…, wait_ms=…)` surface) sees `observed` deterministically,
which is how the round-2 measurements below establish it. The sentence mattered
because it was the evidence a reader would use to conclude the probe-era
exposure window is short — and J3-R2-1 is precisely what lives in that window.

**The one cost, named — and re-measured at round 2** (J3-R2-5). The first ack of
a session waits for the whole probe, which is now seven embeds in four sequenced
waves, two of them at `PROBE_TEXT_BYTES`. Measured across **four fresh sessions**
at the release binary against the live BGE-M3, fresh SQLite each:

| | first ack | probe `serial_items_per_sec` |
| --- | --- | --- |
| round 1 (35-byte probe), reviewer's four runs | 63.0 / 67.5 / 69.6 / 80.2 ms | 54.7 to 75.5 (1.4× spread) |
| round 2 (adds a 1024-byte leg), four runs | **273.5 / 299.3 / 300.4 / 301.1 ms** | 17.95 to 20.54 (**1.14× spread**) |

So **273.5 to 301.1 ms**, and the single "221 ms" this note used to quote was
both wrong and quoted without a warmth caveat for a quantity this workstream
proved is warmth-dependent. The number is ~4× round 1's, and it is worth paying:
it is once per process, bounded by `PROBE_BUDGET` (5 s), roughly **1% of the ~30 s
an MCP client allows for server spawn**, and it buys a rate measured on input the
size the product's own concepts carry. The second column is the unadvertised
gain: embedding a longer text costs more but *varies less*, so the load-bearing
figure's spread across cold repeats fell from 1.4× to 1.14×. Every call after
the first is still 0.074 ms. Admission still refuses to fall back to a constant,
which is what would otherwise remove the wait.

**J3-R2-1's two sides, at the release binary.** Same rig, same live BGE-M3, 200
`lambo_derive` calls back to back and then the pipe closed so `serve` runs its
clean shutdown, then a fresh `serve` on the same SQLite store counts what landed:

| content | commit | acked | close | abandoned | in store after restart |
| --- | --- | --- | --- | --- | --- |
| 512 B | `869b898` (parent) | 68 | 2047 ms (whole budget) | **37** | 31 |
| 512 B | this branch | 4 | 306 ms | **0** | 4 |
| 32 B (round 1's control) | `869b898` | 61 | 1040 ms | 0 | 61 |
| 32 B (round 1's control) | this branch | 4 | 81 ms | **0** | 4 |

The parent's probe read 67.70 items/s on the 512-byte run against a real ~64 ms
service time — the 4× over-estimate the review measured, against a 2× slack —
and 37 of 68 acked writes were abandoned at a **clean** close with no adversary.

**And the throughput this costs, measured rather than waved at.** Four acked out
of 200 is what "the probe sizes nothing load-bearing" means for an *instantaneous
cold* burst, and it is a real cost: on the 32-byte control the parent admitted 61
and applied all 61, where this branch admits 4. It is bounded in time rather than
in kind, which is the reason it is acceptable — the same session, given its first
four writes (~250 ms at this rig), admits the measured depth:

| | lane bound | source | burst acked | close | abandoned | in store |
| --- | --- | --- | --- | --- | --- | --- |
| cold, 512 B | 4 | `probe` | 4 of 200 | 306 ms | 0 | 4 |
| after 4 completed writes, 512 B | **17** | `observed` | **17 of 200** | 1107 ms | **0** | 21 |

`probe_optimism` on that run read **1.14×** — the probe's 18.65 items/s against
the observed 16.33 — where the parent's 35-byte leg was 4.0× out.
**Corrected at round 3 (J3-R3-3): 1.14× did not reproduce.** The round-3
reviewer read the flip line twice at 512 B and got **1.4308 and 1.4312**, and
the arithmetic agrees (a 1024 B embed at 60.2 ms against a 512 B whole-`run` at
~70 ms is 1.43); extrapolated to the top of the declared 700–1500 B band the
ratio runs **1.9× to 2.7×**, because `PROBE_TEXT_BYTES = 1024` sits in the
lower half of that band, not its middle. Under the round-2 design that margin
was load-bearing against the half-budget share; under the round-3 redesign the
ratio is telemetry — quoted now as a range with its content size, and gating
nothing.

**Register sweep (per file, including the nulls).** The claim families this
remediation moved are: the queue bound's width, the probe's provenance, receipt
expiry, the stats key list, and the ordering promise.

| File | Swept for | Result |
| --- | --- | --- |
| `src/writeq.rs` | every numeric claim and stated reason in the constants block, re-derived from the new relations | 6 corrected: `WRITE_QUEUE_DRAIN_BUDGET`'s "one constant serves both" reason; `PROBE_CLAMP_RPS` (512 → 1024); `WRITE_QUEUE_MAX`'s vacuous guard; `RECEIPT_RETENTION`'s false guard; `MAX_RETAINED_RECEIPTS`' memory arithmetic; `PROBE_BUDGET`'s "generous because" reason, which now has a different justification. 3 added (`DRAIN_PROJECTION_SHARE`, `WRITE_QUEUE_LANE_MIN`, `MEASURED_WORST_FLUSH_LAG_SECS`) plus the four probe/observation constants |
| `src/writeq.rs` | the module docs' §Ordering and §Backpressure | both rewritten: §Ordering states the sequential scope (J3-R1-10), §Backpressure states that a measurement has a width and that the probe is an opening estimate |
| `src/mcp/server.rs` | the stats-key block and its "ten keys" neighbourhood, plus the tool instructions | 4 keys added with their reasons; the instruction sentence scoped, with a test asserting the unscoped one is gone |
| `src/memory.rs` | `derive_async_as`'s pre-pass and ordering bullets, and `close`'s quiesce paragraph | 2 corrected (the pre-pass claim, the ordering scope); the quiesce paragraph re-read against the new drain and **still true** — the budget, the forced order and `write_queue_abandoned` are all unchanged |
| `src/mcp/proxy.rs` | any receipt or queue claim | **nothing** — J3 still has no proxy change beyond the one `pub(crate)` const, and `MAX_CONCURRENT_RECEIPT_WAITS`' build guard against `INFLIGHT_DEPTH_WARN` is untouched |
| `tests/serve_proxy_multi_client.rs` | the piggyback assertions J3-R1-9 changes | rewritten: the take-once property is now proven with a second receipt, and *which* response carries a piggyback is no longer asserted (that was a race against the holder's own drain schedule, and it failed under parallel test load) |
| `scripts/observability/dedup_rate.py` | the fact-less-derive message's list of causes | 1 corrected (J3-R1-11); the `n/a`-not-zero behaviour it guards was already right and is left alone |
| `scripts/observability/verify.sh` | whether the J3 line shape is exercised anywhere | 1 step added (J3-R1-12, argued above); 40 → 46 ok |
| `scripts/observability/sample/calls.jsonl`, `make_sample.py` | the J3 line shape | **deliberately unchanged** — byte-identical, for the clean-v1 reason argued above |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | the write-queue key paragraph and the ordering sentence, in both mirrors | 2 passages rewritten in each, verified passage-identical between the mirrors after the edit |
| `README.md`, `AGENTS.md`, `docs/**` beyond `mcp.mdx` | `rg` for `write_queue`, `items_per_sec`, `PROBE_CLAMP`, `WRITE_QUEUE`, `order you sent`, `receipt` | **nothing** — the queue's surface is mirrored only in the two `mcp.mdx` files and this phase doc |
| `scripts/observability/README.md` | the metric-2 note the new `dedup_rate.py` message points at | **checked, unchanged** — it already states the regression twice and says the facts are on the receipt |

### Round-2 remediation (1 P1, 3 P2, 5 P3 — all closed)

`adve-review-mooshik-J3-round2.md` returned REQUEST_CHANGES with all thirteen
round-1 findings verified closed at the artifact and **one new P1**, reached
through the one door the rebuilt admission still left open: the probe's fixed
35-byte `PROBE_TEXT` is not the workload. Measured at the release binary against
the live BGE-M3 with 512-byte concepts, `close()` burned its whole budget and 35
of 77 acked writes were abandoned (I reproduced 37 of 68 on the same rig). The
bound was right about the drain's *width* after round 1; it was still wrong about
its *rate* whenever a concept is longer than a short sentence — and lambo's own
dogfood concepts are, at 700 to 1500 bytes.

**The lesson is the projection lesson's second face: a measurement is only a
bound for the workload it sampled.** Round 1's was "a projection is not a bound";
this one is the same shape one level down. The probe measured a real thing about
a real embedder on real input — just not the input a lane runs.

**The shape chosen, and why it is both of the review's options rather than
either.** The review named two: make the probe's input representative, or floor
the lane bound until observation lands. A measurement decides between them: this
rig's llama-server answers 1280 B in 75.8 ms and returns **HTTP 500 on 8 of 8
calls at 1536 B**. So a probe text sized at the workload band couples the probe's
survival to a deployment's batch configuration, and a probe that fails is
`unmeasured` — a *worse* answer than an optimistic one. A representative probe
therefore cannot carry the guarantee. It can carry the diagnosis, and it does:

| | mechanism | what it buys |
| --- | --- | --- |
| **(b), load-bearing** | `PROBE_LANE_CEILING` / `PROBE_AGGREGATE_CEILING`: while the source is `probe` or `unmeasured`, the bounds are capped at `OBSERVED_MIN_SAMPLES` per lane and `PROBE_CONCURRENCY ×` that in aggregate | the durability property stops depending on the probe's input at all — *admit no more on the probe's word than it takes to replace the probe's word* |
| **(a), additive** | a second timed serial leg at `PROBE_TEXT_BYTES` = 1024, best effort, publishing the slower of the two | the published rate is honest for the product's own workload band (measured `probe_optimism` fell from 4.0× to 1.14×), which is what makes the probe-vs-observed comparison a diagnosis |

Option (c) — scaling the projection by the job's own payload bytes — was
considered and declined: it makes the bound per-job rather than per-lane, so a
refusal message can no longer name a number the caller can reason about, and it
still rests on the probe's rate for the size it did measure.

| # | Finding | Closed by |
| --- | --- | --- |
| **J3-R2-1** (P1) | The probe's text is not the workload; 37 of 68 acked writes abandoned at a clean close, at the binary | `PROBE_LANE_CEILING` / `PROBE_AGGREGATE_CEILING` (the guarantee) plus a representative serial leg (the diagnosis); `PROBE_TEXT`'s false stated reason removed and replaced by the measured length table; `a_burst_of_concepts_larger_than_the_probes_text_still_drains_at_a_clean_close` at 512 B **and** 8192 B |
| **J3-R2-2** (P2) | A fast-failing embedder biases the observed rate upward | `spawn_worker` samples only `Ok` outcomes — the fenced exclusion's own argument, one step further; `a_failed_write_is_never_sampled_into_the_observed_rate` |
| **J3-R2-3** (P2) | The `[~]` box presented three limits as complete | Rewritten above: four limits, with (3) restated at its real magnitude (a lane's worth, not one write) and (4) new — the observed rate is a mean, so mixed concept sizes can still over-admit for the largest |
| **J3-R2-4** (P2) | The two rates that disagree were both published and never compared | `Calibration::probe_serial_items_per_sec` survives the takeover, `probe_optimism()` names the ratio, `write_queue_probe_serial_items_per_sec` publishes it, and one INFO line at the flip carries both numbers; `the_probes_serial_figure_survives_the_observed_rate_that_replaces_it` |
| **J3-R2-5** (P3) | 221 ms did not reproduce; the `observed`-by-the-end claim did not hold | Both corrected above with four fresh-session measurements (273.5 to 301.1 ms) and the real flip condition (four *completed* writes, not four calls) |
| **J3-R2-6** (P3) | Three developer-facing restatements of the ordering promise were unscoped | All three scoped where they stand — `writeq`'s FIFO test, `server.rs`'s chain test, and the Done-when line ~470 lines from the bullet that scopes it; `writeq`'s §Ordering now leads with the scope instead of retracting nine lines later |
| **J3-R2-7** (P3) | `probe_embedder` had no test | Four: the budget covering all `PROBE_EMBEDS` together (asserted at each required leg in turn), the slower-of-two-sizes rule, a refused representative leg, and a hanging one |
| **J3-R2-8** (P3) | Four stated-reason blemishes in the constants block | All four: the "22-25 ms" misquote corrected **and** re-derived against round 2's own live figures; both `lambo_receipt` references replaced with the `lambo_stats(receipt=…)` surface that shipped; the six-space collapse in the `PROBE_CLAMP_RPS` guard message unwrapped; the keys bullet now explains every key added since first landing |
| **J3-R2-9** (P3) | The piggyback test did not prove removal rather than suppression | One assertion on the *later* fetch's piggyback, mutation-checked against a `mark_delivered` that re-pushes what it retained out |

**What round 2 did not change.** `WRITE_QUEUE_MAX`, `MAX_RETAINED_RECEIPTS`,
`PROBE_CLAMP_RPS`, `DRAIN_PROJECTION_SHARE`, `WRITE_QUEUE_DRAIN_BUDGET`,
`RECEIPT_RETENTION` and every receipt constant keep their values and their
derivations; `MEASURED_LOCAL_EMBEDDER_RPS` stays at 141 deliberately, because its
build guard wants the **largest** real rate a local embedder produces and 141 is
that (the representative leg reads lower, ~20 items/s serial, which only widens
the guard's margin). No proxy change. The aggregate bound is still projected from
the probe's concurrent leg after observation takes over — a parallelism *ratio*
applied to the observed serial rate would remove the last of the length bias, and
it was declined this round as a redesign no finding asked for, with limit (2)
already declaring the assumption it rests on.

**Register sweep, round 2 (per file, including the nulls).** The claim families
this round moved are: what the probe's rate is a rate *for*, the two-rate
comparison, the stats key list and its rationale, the ordering promise's scope,
and the Done-when box's completeness.

| File | Swept for | Result |
| --- | --- | --- |
| `src/writeq.rs` | every stated reason that mentions the probe, its text, its budget or its rate, plus every numeric claim in the constants block | 6 corrected: `PROBE_TEXT`'s "not its own input" (the false stated reason under the P1, replaced by the measured length table including the HTTP-500 row); `PROBE_EMBEDS` (6 → 7); `PROBE_BUDGET`'s six-embeds wording and its trade; `DRAIN_PROJECTION_SHARE`'s misquote **and** its now-measured remainder; `MAX_CONCURRENT_RECEIPT_WAITS`' two `lambo_receipt` references; the `PROBE_CLAMP_RPS` guard message's collapsed whitespace. 4 constants added, each with its own build guard |
| `src/writeq.rs` | the module docs' §Ordering and §Backpressure, re-read against round 2 | both edited: §Ordering leads with the scope (J3-R2-6's structural nit); §Backpressure gains "a measurement is only a bound for the workload it sampled" and no longer calls the probe an *opening* estimate, which understated it |
| `src/writeq.rs` | `Calibration`'s doc table and `with_observed_serial`'s "wins outright" reason | both rewritten: the table now has a row per source, because the source decides the ceiling as well as the rate; "wins outright" is qualified — winning is not erasing |
| `src/mcp/server.rs` | the stats-key block, its "ten keys" neighbourhood, and every test asserting a bound value | 1 key added with its reason; `the_stats_payload_reports_the_measured_bound_and_the_drop_count`'s two bound assertions re-derived (they asserted `WRITE_QUEUE_MAX` on a fixture embedder, which is exactly what a probe may no longer authorise); `i1_a_days_worth_of_concurrent_lines_all_parse` given a warm-up that retires the estimate, with the reason stated at the code |
| `src/memory.rs` | `derive_async_as`'s bullets and `close`'s quiesce paragraph, re-read against the new ceilings | **nothing** — the pre-pass claim, the ordering scope and the quiesce budget are all unchanged by round 2, and the quiesce paragraph's numbers are the drain budget's, not the bound's |
| `src/mcp/proxy.rs` | any receipt or queue claim | **nothing**, again — round 2 touches neither the proxy nor `INFLIGHT_DEPTH_WARN`'s guard |
| `tests/serve_proxy_multi_client.rs` | the take-once property's coverage | 1 assertion added (J3-R2-9), mutation-checked |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | the write-queue key paragraph and the "bounds are not constants" paragraph, in both mirrors | 3 passages edited in each, verified passage-identical between the mirrors after the edit; the new paragraph says in operator language why a fresh session's first burst may be refused where a later one is not |
| `scripts/observability/*` | whether any script or its message names a bound, a rate or a probe | **nothing** — `dedup_rate.py`'s J3-R1-11 message names the async ack and the receipt surface, neither of which moved; `verify.sh` stays at 46 ok with `sample/calls.jsonl` still byte-identical |
| `README.md`, `AGENTS.md`, `docs/**` beyond `mcp.mdx` | `rg` for `write_queue`, `items_per_sec`, `PROBE_`, `WRITE_QUEUE`, `lane_bound` | **nothing** — the queue's surface is still mirrored only in the two `mcp.mdx` files and this phase doc |
| `dev-diary/lambo-for-mooshik/J-multi-client.md` | the `[~]` box, the constants table, the probe-cost paragraph, the `observed` claim, and the ordering Done-when line | 5 passages rewritten; the box's limit count went 3 → 4 and the completeness claim is now true |

### Round-3 and the durable-intent redesign (adopted) — 2 P1, 1 P2, 3 P3, all closed

`adve-review-mooshik-J3-round3.md` returned REQUEST_CHANGES with all ten round-2
findings closed at the artifact and **two new P1s the round-2 fix's own shape
opened**, both measured at the release binary against the live BGE-M3: J3-R3-1
(an embedder refusal on the hybrid path returned `Ok` with `embedding = NULL`,
so the observed rate sampled 3 ms non-writes as fast writes — rate inflated
20–45×, **326 and 361 acked writes abandoned** at a clean close) and J3-R3-2
(`PROBE_AGGREGATE_CEILING = 16` was derived per-lane and applied across lanes —
**13 of 16 abandoned from eight concurrent agents up**).

**The remediation is a redesign, commissioned by the operator and specified in
`J3-durability-redesign.md`.** The diagnosis: every round's P1 was one defect
through a different door — the durability invariant was **coupled to an
estimator's correctness**, and the estimator was falsified on a new workload
axis each round (width, warmth, length, failure shape, concurrency scaling —
five axes in three rounds; the series does not converge, because an estimator
is wrong in as many ways as the workload has covariates). The cut:
**durable post-validation intents.** Six staged commits on `wt/j3` (round 1 caught this
list saying five and enumerating five, N9 — the missing one was `66f5aaa`, the register
sweep; the commit whose subject is register accuracy was the one missing from the
register):

1. **`8605f46` — the honesty fix (J3-R3-1's root), standalone because it is
   correct under either design.** An embedder failure or timeout on the hybrid
   derive path is now a hard `Err` before anything is written — never
   applied-with-`NULL`-embedding — closing the located mechanism behind the
   dogfood store's 92/100 unembedded concepts (the `lambo re-embed` backfill
   verb remains required for the existing damage; workstream A's migration
   path). The capability-absent arm stays a degrade (a declared, deterministic,
   session-uniform configuration, not a per-input surprise) — this reverses the
   L82-4-era pin "a dead embedder degrades the write, it does not fail it", and
   the rewritten test records the argument. *Applied ≠ embedded* is first-class:
   `DeriveOutcome.embedded`, `AppliedSummary.embedded` (`Some` only for hybrid
   derive, the one kind that can embed), the receipt sentence reads
   "N created (E embedded)", and the receipt block carries the field. With
   refusals now `Err`, `spawn_worker`'s `is_ok()` sampling filter finally sees
   what it always assumed; pinned through the whole shipping path by
   `an_embedder_refusal_fails_the_write_and_is_never_sampled`, red at the
   pre-fix hybrid (it settled `Applied("1 created (0 embedded)")` — the exact
   dishonesty).
2. **`e7ff6f2` — durable post-validation intents.** On ack, `admit` records the
   validated job (concepts, interaction id, receipt id, lane seq) as
   `Mutation::PutWriteIntent` through the ordinary write-behind log, under a
   graph⊃lanes lock nesting that puts the intent in the log before a worker can
   see the job. The C-series "session closed, tail durable" final flush carries
   it unchanged, so **acked ⇒ (applied ∨ durable intent) at a clean close, by
   construction**. Applying a job consumes its intent **in the same write-lock
   critical section as the commit** (hybrid: a `CommitHook` at the
   epoch-checked commit; canonical/action: inline under the same guard) — the
   flush drain takes that same lock, so apply + consume always travel in one
   batch, i.e. one store transaction, and a crash can never leave a write
   durable beside its unconsumed intent: replay is idempotent per receipt id by
   construction. A clean close defers what it cannot drain (`intent_durable`
   receipts, `write_queue_deferred`) instead of abandoning it; the next attach
   replays unconsumed intents strictly sequentially in (`issued_ms`,
   `lane_seq`) order; consumed rows are retained for `RECEIPT_RETENTION`
   (const-asserted equal to `types::WRITE_INTENT_RETENTION`) so the original
   receipt id answers `applied_after_restart`, agent-scoped, in the new
   process. The crash window is unchanged: a `kill -9` loses unflushed intents
   exactly as it loses the rest of the tail, and receipts stay honest.
3. **`9e48dca` — the estimator demoted.** Admission bounds are static and
   structural: `WRITE_QUEUE_MAX` (1024, the receipt-store memory cap, unchanged)
   and the new `WRITE_QUEUE_LANE_MAX = WRITE_QUEUE_MAX /
   MAX_CONCURRENT_RECEIPT_WAITS = 64`, one agent's fair share — a division of
   two existing structural constants, not a projection of any rate. Deleted
   with the argument recorded at the site: `DRAIN_PROJECTION_SHARE`,
   `WRITE_QUEUE_MIN`, `WRITE_QUEUE_LANE_MIN`, `PROBE_LANE_CEILING`,
   `PROBE_AGGREGATE_CEILING`, `project()`, and `await_calibration` (admission
   is instant; the first ack no longer waits on the probe). The probe/observed
   apparatus survives as telemetry only — both rates, the slower-of-two
   publication, the observed takeover, `probe_optimism`, the flip line — none
   of it sizing anything.
4. **`2bba0e9` — the proof obligations at the shipped binary** (below).
5. **`5ef7038` — the docs**: this note, the design doc's as-built section, and both
   `mcp.mdx` mirrors.
6. **`66f5aaa` — the register sweep**: the three estimator-era stated reasons the
   demotion left behind.

**The invariant, demonstrated at the release binary against the live BGE-M3**
(`evidence/mooshik-j3-durable-intents/`, script committed beside the transcript;
store counts read back with sqlite3; every durability figure reads the
**embedding column**, never `applied` counts). Sixteen agents × four derives of
1024-byte concepts — the multi-agent, in-band-size regime both round-3 P1s
lived in — closed immediately:

| | acked | clean close | applied w/ embedding | durable intents | lost |
| --- | --- | --- | --- | --- | --- |
| **red** (round 3, `ed22476`, same rig) | 365 / 400 / 16 | whole budget | — | — | **326 / 361 / 13 of 16** |
| **green** (this branch) | 64 | 2.04 s | 1 | 63 | **0** — 64 == 1 + 63, exactly |

The next serve replayed all 63 (`write_queue_replayed: 63`), the original
receipt ids answered `applied_after_restart`, and the final store held all 64
concepts **with their vectors** (0 `embedding IS NULL` rows at every readback,
0 unconsumed intents). The J3-R3-1 refusal probe (a 3000-byte content this
llama refuses) settled `failed` — "nothing was written" — with no row written.

**The proof obligations, disposed one by one** (`J3-durability-redesign.md`):

1. *Acked ⇒ (applied ∨ durable intent) at a clean close, realistic sizes,
   multi-agent* — the live run above; at `Memory` level
   (`an_acked_write_survives_a_clean_close_as_a_durable_intent_and_replays`);
   at the pipeline in regimes that deliberately outrun the budget
   (`a_close_that_cannot_drain_defers_acked_writes_as_durable_intents`,
   `one_agents_burst_never_loses_an_acked_write_at_a_clean_close`,
   `a_burst_of_concepts_larger_than_the_probes_text_loses_nothing_at_a_clean_close`
   at 512 B and 8 KiB).
2. *Replay: kill −9 idempotent, per-lane order, contract enforced, the truth
   table* — `tests/serve_intent_durability.rs` at the shipped binary:
   `a_kill_nine_mid_replay_re_replays_idempotently` (exactly-once judged at the
   embedding column and at Derives reinforcements, for every crash/flush
   interleaving) and
   `seeded_intents_replay_in_lane_order_and_answer_applied_after_restart`
   (order pinned by a create-then-match pair whose receipts would flip under
   inversion; `forbidden` across agents; replay rides no admission). The
   embedding contract at replay is enforced by construction — replay runs
   `hybrid::derive`, whose `ensure_compatible` gate precedes every embed.
3. *Applied ≠ embedded* — commit 1, and every durability assertion in the new
   tests reads the store's embedding column.
4. *The e-process breaker* — **deferred with its seam**, and the argument is
   recorded at commit 3: the Part-2 math (ACI conformal bounds, the e-process)
   was specified for an admission that still estimates, and the static bounds
   dissolved that premise. The seam left ready is the retained probe/observed
   telemetry pair plus `probe_optimism` — exactly the divergence statistic an
   e-process would consume — and nothing acts on it today because nothing needs
   to: no estimate gates durability.
5. *Intents ride the ledger* — deferred to J4, whose completion-line schema is
   the vehicle the design names (the same disposition as the declared metric-2
   regression above); the `write_intents` table itself is queryable meanwhile.

**Open questions from the design doc, decided at the decision sites:**

* **Intent placement**: a new mutation kind in the write-behind log, not a
  sibling path — it reuses the drain, the final flush, the fencing token and
  the batch transaction wholesale, and the schema isolation is a table either
  way (`write_intents` in both SQL adapters; snapshot rows in `MemoryStore`;
  expired consumed rows are purged inside the consume step, clocked by the
  mutation's own `consumed_at`, so no adapter grows a clock).
* **Replay throttling**: sequential background replay, at most one write in
  flight, deliberately **not** through admission — admission-routed replay
  would answer `lane_full` to the very calls the restart interrupted, the
  starvation the design forbids; a replayed intent already paid for admission
  in the session that acked it. The accepted cost, documented at the site: a
  fresh write can land before a replayed intent from the same agent —
  cross-restart interleaving is unordered, the same scope §Ordering already
  declares for concurrent submissions.
* **Receipt-store ownership across restart**: the intent record *is* the
  durable half of the receipt store — unconsumed rows answer `pending`,
  consumed ones `applied_after_restart`/`failed` for one retention window,
  and nothing else survives: `restart_lost` remains the honest answer for
  receipts with no durable record.
* **Does the proxy need to know?** No — verified, not just believed: intents
  are holder-internal (the proxy forwards tool responses byte-for-byte, and
  `src/mcp/proxy.rs` has no J3-round-3 change), and the -32002 wording's
  "outcome UNKNOWN — recall before re-deriving" remains correct for the case
  it covers, while receipts whose intent record survived now do better.
* **Lease interplay** (not in the doc's list, decided in passing): a fenced
  holder never consumes — its flushes are refused at the token — so a
  pre-fence durable intent is applied exactly once, by the current holder's
  replay; the fenced worker's receipt message says so.

**The round-3 findings, one by one:**

| # | Finding | Closed by |
| --- | --- | --- |
| **J3-R3-1** (P1) | The observed rate sampled writes that never embedded; 326/361 abandoned | Commit 1 (refusal is an `Err` at the source, so the sampling filter's premise is finally true) + commit 2 (abandonment itself is gone); the estimator half is moot under commit 3 — no rate sizes a bound |
| **J3-R3-2** (P1) | The aggregate ceiling derived per-lane, applied across lanes; 13/16 abandoned from 8 agents up | **Closed by deletion, with the argument recorded where the ceiling lived**: fairness needs a share, not a projection — `WRITE_QUEUE_LANE_MAX` is per-lane by construction and `WRITE_QUEUE_MAX` is a memory cap, and neither can abandon anything (commit 2) |
| **J3-R3-3** (P2) | The `probe_optimism` "fell to 1.14×" claim did not reproduce (1.43× measured); 1024 B is the band's lower half, not its middle | Corrected above at the claim, with the reviewer's figures, the agreeing arithmetic, and the 1.9–2.7× band-top extrapolation; the ratio is telemetry now and gates nothing |
| **J3-R3-4** (P3) | The refusal message attributed the bound to a measured rate in the era a ceiling decided | `DropReason::describe` rewritten: the lane refusal names the per-agent fair share (1/16 of the queue) and the queue refusal names the memory cap — no refusal claims to be "measured on this deployment's embedder" in any era |
| **J3-R3-5** (P3) | The 22–25 ms misquote fixed at the cited line, surviving at three siblings | All three: the `writeq` module header and `PROBE_CLAMP_RPS` (commit 3, which also updated the clamp's operator-reference figures to the slower-of-two ~18–21 items/s reading), and this file's §J3 opening line (this commit) |
| **J3-R3-6** (P3) | Limit (4) of the `[~]` box inherited limit (3)'s magnitude, understating its own by up to 256× | The box is rewritten wholesale below — the limits it enumerated were properties of the estimator design, and the rewritten box states the redesign's own limits at their own magnitudes |

**Constants after the redesign** (current; the earlier table is the round-2
record):

| Constant | Value | Derived from |
| --- | --- | --- |
| `WRITE_QUEUE_DRAIN_BUDGET` | 2 s | Unchanged value, demoted role: how long a clean close drains before **deferring** the remainder — a latency price, not a durability deadline. Still carved out of `CLOSE_FLUSH_GRACE` (8 s), still build-guarded |
| `WRITE_QUEUE_MAX` | 1024 | Unchanged: `MAX_RETAINED_RECEIPTS / 4`, the receipt-store memory cap — the whole-queue bound for every source |
| `WRITE_QUEUE_LANE_MAX` | 64 | **New**: `WRITE_QUEUE_MAX / MAX_CONCURRENT_RECEIPT_WAITS` — one agent's fair share of the queue, 1/16 by the constant that already declares the multi-caller design point. A fairness rule from two structural constants; being wrong costs a refusal or a deferral, never a loss |
| `WRITE_QUEUE_MAX_BYTES` | 16 MiB | Unchanged: the byte cap, a count being the wrong unit for memory |
| `PROBE_CLAMP_RPS` | 1024 | Re-derived as `WRITE_QUEUE_MAX` per second — a telemetry sanitization clamp for fixture-fast readings, numerically unchanged; its guard above `3 × MEASURED_LOCAL_EMBEDDER_RPS` stays |
| `WRITE_INTENT_RETENTION` | 300 s | **New** (`types::`): how long a consumed intent row survives — the cross-restart receipt window, const-asserted equal to `RECEIPT_RETENTION` because a receipt's answer must not depend on whether a restart intervened |
| probe/observation constants | unchanged | `PROBE_TEXT`, `PROBE_TEXT_BYTES`, `PROBE_CONCURRENCY`, `PROBE_WARMUP_EMBEDS`, `PROBE_EMBEDS`, `PROBE_BUDGET`, `OBSERVED_MIN_SAMPLES`, `OBSERVED_EWMA_WEIGHT`, `MEASURED_LOCAL_EMBEDDER_RPS` — all telemetry now; nothing awaits the probe and nothing projects its rates |
| receipt constants | unchanged | `RECEIPT_RETENTION`, `MAX_RETAINED_RECEIPTS`, `MAX_RECEIPT_IDS`, `RECEIPT_WAIT_MAX`, `MAX_CONCURRENT_RECEIPT_WAITS`, `MAX_PIGGYBACK_RECEIPTS` |

**Register sweep, round 3 (per file, including the nulls).** The claim families
this round moved: what bounds rest on, what a close does with the remainder,
the receipt truth table, the applied/embedded distinction, and the stats key
list.

| File | Swept for | Result |
| --- | --- | --- |
| `src/writeq.rs` | every stated reason naming a projection, a ceiling, a share, or the close's abandonment; the module doc's §Backpressure; the `Calibration` table | rewritten to the fairness/memory role with the estimator history recorded in place; the deleted constants' arguments recorded at the site that held them; the two J3-R3-5 sibling misquotes corrected |
| `src/graph/hybrid.rs` | the degrade taxonomy | module doc, `Resolution` doc and `derive`'s doc rewritten: embed failure fails the call; capability absence still degrades; the L82-4 test pin reversed with the argument |
| `src/mcp/server.rs` | the stats-key comments and the key-list test | bound keys re-annotated as static caps; two keys added; the fifteen-key enumerations grown to seventeen |
| `src/memory.rs` | `close()`'s quiesce paragraph and the build path | quiesce comment now states the defer semantics; replay spawn and stop documented at their sites |
| `src/mcp/proxy.rs` | any intent, replay or receipt-state claim | **nothing** — no round-3 change; the -32002 wording remains correct for the record-less case |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | the receipt-state table, the write-queue key list, the bounds paragraph, the rates paragraph, the dropped paragraph | 5 passages rewritten in each, byte-identical between the mirrors (verified by diffing the two change bodies) |
| `migrations/sqlite/001_init.sql`, `migrations/cockroach/001_init.sql` | the new table | `write_intents` DDL added to both, with the lifecycle documented at the DDL |
| `scripts/observability/*` | any bound, rate or receipt-state claim | **nothing** — `verify.sh` stays at 46 ok, `sample/calls.jsonl` byte-identical; the ledger completion-line repair remains handed to J4 |
| this file | the §J3 opening line, the status header, the close bullet, the keys bullet, the constants table, the round-2 `probe_optimism` claim, the `[~]` Done-when box | all rewritten or annotated; history kept as history with the current truth stated beside it |

**Gates after the five commits (repo-wide, house convention):**

| Gate | Result |
| --- | --- |
| `cargo test --all --features fixtures` | **901 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | **991 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | **563 / 0 / 0** |
| `scripts/observability/verify.sh` | **46 ok** |

(Baselines at `ed22476`: 898 / 986 / 564 / 46. The −1 in the cockroach and
lib-side counts is two calibration-bound tests merged into
`no_rate_can_move_the_bounds`; everything else is additions.)

### Round-1 review remediation (`adve-review-mooshik-J3-redesign-round1.md`)

Round 1 upheld the founding invariant at the binary and closed all six round-3
findings, then returned **REQUEST_CHANGES** on the durable half's *promise*:
1 P1, 3 P2, 1 unconfirmed P2 candidate, 9 P3. Dispositions below, in the order
they were worked.

**F5 — an un-migrated store: CONFIRMED, and not the shape the review
suspected.** The reviewer observed a `serve` acking 64 writes against a store
with no `write_intents` table but could not read the store files to settle the
mechanism, and named `IF NOT EXISTS` as the likely benign explanation. Settled
by construction and by driving the real binary:

* `init_schema` is called from **exactly one place in the product** —
  `src/cli/provision.rs:29`, i.e. `lambo provision`. Nothing on the attach path
  calls it, so `IF NOT EXISTS` never runs at attach and cannot be the answer.
  "Provisioned by the previous build, binary upgraded, never re-provisioned" is
  therefore reachable through the product's own path — and this branch is the
  one that made it bite, because `write_intents` is a **new table** (the only
  schema change on the branch: one `CREATE TABLE` per adapter).
* Driven at the serve binary against such a store: it **attached, acked four
  derives, settled two of them `applied`, reported `degraded=false
  dead_lettered=0` for the whole session — and left `concepts=0 embedded=0`.**
  The loss is **total, not intent-scoped**: a write's mutations and its
  `PutWriteIntent` share one flush transaction, so one missing table rolls every
  batch back whole. That is worse than the review guessed.
* It is **not silent**, which is why it does not outrank N1: the close fails at
  three log sites (`close: final flush failed; 24 tail mutations returned`,
  `final flush failed — tail lost on exit`, `Memory dropped after a close() that
  did not finish`) and `serve` exits non-zero naming the missing table. The
  founding invariant is scoped to a **clean** close, and this close is not one —
  its precondition is violated loudly rather than the invariant being satisfied
  falsely. Graded **P2**: no in-session signal, an ack surface that says
  "accepted" and "applied" throughout, and total durability loss.
* **Fixed, not just recorded.** `GraphStore::preflight_schema` (default
  `Ok(())`, so no test double changes) diffs the tables present in the store
  against the table names **derived from the DDL the build ships**
  (`store::tables_in_ddl` over the already-`include_str!`d migration) — so the
  check cannot drift behind the next schema addition the way `write_intents`
  drifted past every existing path. Implemented for both SQL adapters
  (`sqlite_master`; `information_schema.tables`), called at
  `Memory::builder().build()` **before the lease acquire** so a refusal leaves no
  lease row to strand the session for a TTL, and returning
  `StoreError::Capability` with a message that names the missing table and
  `lambo provision`. Refusal beats the alternative of disabling only the async
  ack path: with the table absent the whole batch fails, so a "degraded" mode
  would keep the invariant silently void — exactly F5's complaint.
* **Not covered, stated plainly:** missing *columns*. Those converge through
  `init_schema`'s `ensure_column` / `ADD COLUMN IF NOT EXISTS` ladders, which
  are on the same provision-only path. The preflight checks tables.
* Pinned three ways: at the binary
  (`an_unmigrated_store_refuses_the_attach_naming_lambo_provision`, which also
  asserts no lease row is left), at the adapter
  (`preflight_schema_refuses_an_unprovisioned_or_unmigrated_target`), and at the
  derivation (`ddl_table_names_are_derived_from_the_shipped_migration`, which
  fails if a DDL rewrite ever empties the required set).

**Register sweep, F5 (per file, including the nulls).**

| File | Swept for | Result |
| --- | --- | --- |
| `src/store/mod.rs` | any claim about when the schema is created or converged | the new trait method states that `init_schema` is provision-only and that the check covers **tables, not columns**; `tables_in_ddl` states which DDL idiom it matches and that it deliberately matches no other |
| `src/store/sqlite.rs` | the same, plus the inline `include_str!` | the migration hoisted to `INIT_SQL` (one authority for execution and for the preflight); `preflight_schema` states its cost (one statement, no DDL, no write) and its lease ordering |
| `src/store/cockroach.rs` | the same | `preflight_schema` added, noting that Cockroach is provisioned by `scripts/provision.sh` — so the hazard is the same — and that here each statement is a round trip |
| `src/memory.rs` | the attach-path comment ladder | step (0a) added ahead of the lease with the measured consequence and the ordering reason recorded at the site |
| `src/cli/provision.rs` | its "idempotent" claim | **nothing** — the claim is true and is now the thing the refusal points at |
| `migrations/*/001_init.sql` | the DDL idiom the preflight reads | **nothing** — both files already use `CREATE TABLE IF NOT EXISTS` uniformly; a unit test now fails if that stops being true |

**N1 (P1) — a transient embedder outage at attach no longer destroys the backlog.**
The mechanism the review measured: `spawn_replay` ran unconditionally at attach and its
failure arm consumed **every** failed intent as `failed`, with no discrimination between a
content refusal and an unreachable or timed-out embedder — a distinction this branch created
by making the timeout an `Err`. A 63-intent backlog therefore burned in ~31 minutes against
a *hanging* embedder (one `HYBRID_IO_TIMEOUT` each) or in ~2 minutes against a dead one, and
J2's lease arithmetic aims the attach at exactly the unhealthy window. Fixed in four parts,
in the review's dependency order:

1. **A liveness gate.** One embed of `PROBE_TEXT` (35 bytes, chosen because every embedder
   accepts it) against `PROBE_BUDGET`, before the loop. On failure: one `tracing::warn!`
   naming the session and the backlog, and `return` — **nothing consumed**, every intent
   still durable and `pending_replay`. A dead or hanging embedder at attach now costs one
   embed, not one timeout per intent.
2. **A structural classification, not a string match.** The failure arm consumes only
   `LamboError::Embed` — "the embedder answered and the answer is unusable for *this
   content*" — and leaves everything else unconsumed, ending the loop. The type carries the
   fact rather than the message: `EmbedError::is_transient` classifies at the site that
   knows the cause (`Unavailable` = never reached, `Backend` = answered and unusable), and
   the new `LamboError::EmbedUnavailable` carries it out of `hybrid::derive`. This follows
   **J1-R2-2's precedent exactly** — `SoftLock` was split from `Conflict` for the same
   reason, and like that split the `Display`, the `err_class` and therefore the receipt text
   and the ledger's `error_kind` are unchanged. Where the classification is imprecise it
   says so at the type: a `503`-while-loading lands in `Backend` and is called permanent,
   because HTTP does not let a caller tell it from a refusal — bounded by the liveness gate
   (a whole outage never reaches the classifier) and by costing at most the one intent that
   met the fault.
3. **The design's `attempts`-column bound argued down, not shipped** — see the design doc's
   deviations section for the argument. In short: the poison case is consumed on the first
   attempt by (2), a transient failure now *ends* the loop, and what remains is a debt that
   should be visible rather than deleted. Plus one branch-specific reason: a new column is
   precisely the schema change the F5 preflight cannot see.
4. **The restatements.** Done-when limit (4) now states the class boundary and the magnitude
   it had wrong, and a fifth limit records the crash-blocks-replay window the review
   measured live; `IntentRecorded::describe()` says the next serve will **re-attempt** the
   write, not "will apply it", and names the three answers that can follow; the design doc's
   as-built section carries the deviation; both `mcp.mdx` mirrors carry the `intent_durable`
   wording and the new state.

**What the operator sees, which is the other half of the answer** — a never-consumed intent
must not become an invisible retry loop. Per attach: one warn line naming the backlog and
the reason. Per receipt: `pending_replay`, a **new** state (N8) whose `describe()` says the
write was admitted by an earlier serve and may settle in a later one — the taxonomy's one
tag collision, since a live `pending` settles in ~27 ms and a replay-owed one can outlive
several processes. Per session: `write_queue_replay_owed`, the eighteenth unconditional
`lambo_stats` key, holding the debt — non-zero while `write_queue_replayed` does not move is
the readable form of "the embedder was not answering at attach and nothing was discarded".

Pinned red-first at `Memory` level, both directions, and both verified red by mutating the
two new guards off (`if false &&` on the liveness gate and on the classification), which
fails on `write_queue_replay_owed` being silently discharged to 0:

* `a_dead_embedder_at_attach_leaves_the_backlog_durable_and_a_later_serve_applies_it` —
  session 1 defers an intent, session 2 attaches against an embedder that refuses even the
  probe (`EmbedError::Unavailable`, the shipped adapter's transport error) and must leave the
  intent unconsumed and answering `pending_replay`, and session 3 applies it **with its
  embedding**. The last leg is the point: the write an outage would have destroyed is still
  a write.
* `a_content_refusal_at_replay_still_settles_the_intent_failed` — a probe-answering embedder
  that refuses real content with `EmbedError::Backend` (the shape of a llama.cpp `500`) must
  still consume the intent `failed`, so the fix does not trade "never retry" for "retry
  forever".

**And red-first at the release binary against the live BGE-M3, which is where the house
standard puts the burden** (`evidence/mooshik-j3-durable-intents/n1-outage-run-2026-08-21.txt`,
driver `j3_n1_outage_demo.py` beside it, run unmodified against both binaries; llama.cpp
health verified before each). The outage is the real adapter's own transport failure —
`llama_url` at `127.0.0.1:9`, closed, producing `EmbedError::Unavailable` — not a double.
Same 16 × 4 × 1024 B shape, then an attach during the outage, then an attach healthy:

| | session 1 | session 2, embedder unreachable | session 3, embedder back |
| --- | --- | --- | --- |
| **red** (`ed03266`, the reviewed commit) | 64 acked, 63 durable intents | **unconsumed=0, failed_rows=63**, sampled receipt `failed`, `replay_owed` key absent | nothing left to replay; final store `concept_count=1` |
| **green** (this remediation) | 64 acked, 62 durable intents | unconsumed=**62**, failed_rows=**0**, sampled receipt `pending_replay`, `replay_owed=62` | replayed=62, `applied_after_restart`, store **64 embedded / 0 NULL / 0 unconsumed** |

Sixty-three acked writes destroyed by one attach that happened during an outage of a
dependency the writes did not need in order to be *recorded* — each of them previously told
"recorded as a DURABLE INTENT and the next serve of this session will apply it" — against a
backlog that now survives the same outage whole and lands with its vectors at the next
healthy attach. The 63-vs-62 difference is drain timing (the burst races the embedder), not
behaviour: both runs end at 64 acked and 64 embedded.

The healthy path is unchanged, checked with the branch's own unmodified driver at a binary
built from the remediated tree: `j3_live_demo.py` still reads
**64 == 1 + 63, all 63 replayed, 64 embedded / 0 NULL / 0 unconsumed, OVERALL PASS** — so the
liveness gate costs the healthy attach nothing measurable.

**Register sweep, N1 + N8 + N9 + F1's enum count (per file, including the nulls).**

| File | Swept for | Result |
| --- | --- | --- |
| `src/embed/mod.rs` | every claim about what an embed failure means | `EmbedError::is_transient` added with the axis, both variants' meanings, and the two places it is imprecise (the `503` case, CON-7's empty-text guard) stated at the method rather than left to be discovered |
| `src/types/mod.rs` | `LamboError`'s variant docs | `Embed`'s doc now says "for this input"; `EmbedUnavailable` records the split, the J1-R2-2 precedent, its only two producers, and that `Display` is identical on purpose |
| `src/graph/hybrid.rs` | `derive`'s docstring and both failure arms' in-comments | the docstring names which type each failure produces and why; the timeout arm's comment records that only the type changed, not the message |
| `src/mcp/server.rs` | `err_class`, the stats payload comments, the key-list test | the two embed variants share one class, annotated with the same pairing note the `Conflict`/`SoftLock` arm carries; the new key documented as a depth, not a total |
| `src/writeq.rs` | `spawn_replay`'s doc bullets, `ReceiptAnswer`'s docstring and `describe`s, the counters' docs | the **Failure** bullet rewritten around the class boundary and a **Liveness** bullet added; the enum docstring's count corrected (F1) and now enumerates all eleven; `IntentRecorded::describe` no longer promises application; `PendingReplay` documented as the distinct wait it is; `replay_owed` documented as the only counter that can go down |
| `src/mcp/proxy.rs` | any receipt-state claim, since a state was added | **nothing** — the proxy renders whatever the holder answers and its -32002 wording is about record-*less* receipts, which `pending_replay` is not |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | the receipt-state table, the `intent_durable` promise, the write-queue key list and the paragraph that reads it | 4 passages in each, change bodies diffed and identical between the mirrors |
| `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` | the commit count, the deviations, the unconsumed-row answer | five staged commits → six (N9); the timeout deviation's unpaid debts recorded as paid; the new N1 deviation and the argued-down `attempts` bound written in full |
| this file | the Done-when box's limits (4) and the receipt-state count, the seventeen-keys bullet, the §J3 commit list | limit (4) restated at its own magnitude with the class boundary, limit (5) added; "seven states" → eleven; the key bullet grown to eighteen and the new key explained; the commit list grown to six |
| `scripts/observability/*` | the new stats key and the new receipt state | **nothing** — `verify.sh` asserts on ledger and heartbeat shapes, not on the write-queue key set; still 46 ok |

**N2 (P2) — the deviation's own arm is pinned.** The branch's one deviation from the design
of record made an embed **timeout** an `Err` as well as a refusal, argued it, and shipped it
without a test — while it is the commoner field condition of the two, a slow or wedged
llama.cpp being likelier than a refusing one. `an_embed_timeout_fails_the_write_and_writes_
nothing` now drives an embedder that never resolves under
`#[tokio::test(start_paused = true)]`: tokio auto-advances a paused clock to the next
deadline once every task is idle, so a 30-second `HYBRID_IO_TIMEOUT` fires **immediately**
and the test runs in 0.00 s — a property test, not a stopwatch. It asserts the four things
the refusal arm's pin asserts, and one more the classification now requires:
`LamboError::EmbedUnavailable` (not `Embed` — a timeout is an unreachable embedder), the
message naming "timed out", "nothing was written", `node_count() == 1`, and
`embedding().is_none()` (MINOR-2: a failed embed must not bind the session's contract). Its
sibling `a_content_refusal_stays_an_embed_error_not_an_unavailable_one` pins the other side,
and the pre-existing `embed_failure_fails_the_write_and_writes_nothing` was sharpened rather
than left ambiguous: the failure it drives is `Unavailable("server down")`, so the class it
must produce is `EmbedUnavailable`. The reversed pin now covers all three arms — refusal,
timeout, and transport — where §J3's round-3 register row could only honestly say "the L82-4
test pin reversed", singular.

**F3 (P2) — the reversed pin's blast radius, now stated where it is read; the behaviour
deliberately unchanged.** The review's finding was not that the behaviour is wrong but that
its *boundary* was misstated and its *availability* consequence undeclared. Both halves
answered:

* **The misstatement**, at the source that makes the claim: `hybrid.rs`'s module doc now says
  outright that "the capability-absent arm stays a degrade" is a claim about the **store**,
  that `SqliteStore` advertises `VECTOR_SEARCH` unconditionally, that `build_embedder`
  yields an embedder or a startup error so no embedder-absent state reaches `derive`, that
  `Hybrid` is the config default — and therefore that **no arm degrades on a dead
  embedder**.
* **The declaration**, where a user reads it: a new paragraph in both `mcp.mdx` mirrors under
  `lambo_derive` and limit (6) of the Done-when box, both naming the consequence (a server
  acked — a write not yet attempted when the session closes waits for an embedder as a
  durable intent, while a write the worker reaches during the outage fails and its receipt
  says so (the honest asymmetry, J3-R2R-2), which is exactly what N1's fix changed the
  replay path to preserve), and the opt-out
  (`match_strategy = "canonical"`).
* **The behaviour: argued and kept.** The review's second half asked whether a
  connection-level failure deserves the same "declared, session-uniform" treatment the
  absent store capability gets. It does not, and the reason is the same one N1 just
  established at the type level: the classification can separate *refused this content* from
  *not reached*, and nothing can separate *not reached, transiently* from *not reached, for
  the rest of this session*. Promoting the second to a session-wide keyword-only degrade
  would mean guessing that the embedder is gone for good — and guessing wrong writes
  exactly the unfindable `embedding: NULL` concepts the honesty fix exists to end, silently,
  session-wide. Spec §3.2's lawful degraded mode is reachable, and it is reachable the only
  honest way: by declaration.

**Register sweep, N2 + F3 (per file, including the nulls).**

| File | Swept for | Result |
| --- | --- | --- |
| `src/graph/hybrid.rs` | the degrade taxonomy's boundary claim, and the coverage claim about the reversed pin | the module doc now states the store/embedder boundary explicitly and why a per-call failure cannot be promoted to a session-uniform degrade; three arms pinned where one was |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | any statement of what a derive needs and what happens when it is missing | one paragraph added under `lambo_derive` in each, change bodies diffed and identical between the mirrors |
| `src/types/mod.rs`, `src/config.rs` | whether `match_strategy`'s own docs support the opt-out F3 now points users at | **a finding of its own, fixed.** `MatchStrategy`'s docstring read "Which concepts a **recall** is allowed to match" and its two variants described recall only — while the same setting decides whether a *derive* embeds, which is the half with the availability consequence and the half F3's remediation tells a user to change. Sending someone to a write-path setting documented as a read-path filter is the same false-stated-reason family this review round exists for. Both variants now document both axes, and one live trap is recorded beside them: the `Default` impl is `Canonical` while `Config::default()` is `Hybrid`, so the attribute and the product disagree and only the config answers "what does a deployment do". The `Config` field, which had **no** docstring, points at it |
| `src/embed/mod.rs` | whether `build_embedder`'s "always an embedder or a startup error" is stated where the degrade claim is made | **nothing at the source** — it is true and unchanged; what was missing was the *consequence*, now recorded at `hybrid.rs` where the claim it falsifies lives |
| this file | the Done-when limits | limit (6) added, at its own magnitude, with the behaviour argued rather than only described |

**N3 (P2) — the twelve estimator-era stated reasons, including the two that operators read.**
The round-3 register sweep named `writeq.rs` first and claimed it was swept for "every stated
reason naming a projection, a ceiling, a share, or the close's abandonment". The module doc's
§Backpressure genuinely was; the item-level docstrings were not, and two of the survivors
were not prose at all. All twelve corrected, each with the false sentence quoted at the site
so the correction is auditable rather than a silent overwrite:

| Site | What it claimed | Now |
| --- | --- | --- |
| `WritePipeline::spawn`'s probe `info!` — **every session start** | "bounds measured on this deployment's embedder — the lane bound from the serial leg, the aggregate from the concurrent one" | provenance first: the bounds are static (lane 64 / queue 1024) and no rate moves them; the rates are telemetry, named with their widths |
| the probe-failure `warn!` | "the bound is the **unmeasured floor**" — `WRITE_QUEUE_MIN`, which **this branch deleted** | says there is no rate telemetry this session, that the bounds are unaffected and never came from the probe, and — since a failed probe is a real signal about something else — that with `match_strategy=hybrid` an embedder that cannot answer will also fail every derive (F3's warning where it is actually useful) |
| `WRITE_QUEUE_MAX`'s headline | "Upper clamp on **the measured bound**" | "The queue's aggregate admission bound" — there is nothing left for it to clamp |
| the `PROBE_CLAMP_RPS` build-assert message | "or the queue bound stops being a per-deployment measurement and **becomes a constant**" — the intended state | rewritten as telemetry hygiene, and the guard now constrains only that (see N4) |
| `PROBE_CONCURRENCY` | "It **sizes the aggregate bound only** — the per-lane bound comes from the serial leg" | it sizes nothing; it is the width of a telemetry reading, reported beside the serial one so the two are comparable |
| `OBSERVED_MIN_SAMPLES` | "a probe that failed outright and **floored the bound**" | a probe that failed leaves the session with no rate telemetry; the floor it named is deleted |
| `OBSERVED_EWMA_WEIGHT` | "A weight of 1 would make **the bound** track a single slow write" | the published *rate*; both failure modes are telemetry faults now, and the reason to smooth is stated (a 1-weight rate means only "the last write") |
| `WritePipeline::spawn`'s docstring | "It is nonetheless **the only source of the bound** — admission awaits its result rather than falling back to a constant" — both halves false | it sources nothing and `admit` never consults it; still budgeted, for the reason it survives at all |
| `lane_outstanding` | "the population `Calibration::lane_bound` **bounds**" | the population `WRITE_QUEUE_LANE_MAX` bounds — `Calibration::lane_bound` is a field that copies the constant — with the round-3 defect (derived per-lane, enforced across lanes) named as what the distinction is for |
| the worker's timing comment | "*is* the serial service time **the admission bound needs**" | is this deployment's serial service time, the figure `write_queue_serial_items_per_sec` publishes, feeding no admission decision |
| `probe_embedder` leg 2 | "what `Calibration::lane_bound` is **projected from**" — `project()` was deleted | what the key reports; projected into nothing, and `from_rates` hardcodes the static for every source including `Unmeasured` |
| `probe_embedder` leg 4 | "for the **aggregate bound**" | for the aggregate rate and the parallelism figure an operator reads |

**N4 (P3) — the deleted estimator no longer sizes the bounds at build time.** The chain was
real and live: `PROBE_CLAMP_RPS = WRITE_QUEUE_MAX as u64`, a `const_assert` requiring
`PROBE_CLAMP_RPS > 3 × MEASURED_LOCAL_EMBEDDER_RPS` (141 items/s, this rig's llama.cpp), and
`MAX_RETAINED_RECEIPTS`'s own stated reason deriving 4096 *from that inequality* — so both
surviving bounds were structural in kind and **measured in magnitude**, and an edit shrinking
the receipt cap for memory reasons would have failed the build citing a rationale the branch
declares retired. Cut at the coupling, not at the guard: `PROBE_CLAMP_RPS` is now its own
literal `1_024` (the value is unchanged, so no reading moves), the assert survives as pure
telemetry hygiene with no bound in its message, and `MAX_RETAINED_RECEIPTS` states the
derivation it actually has — 4096 is what the ≈31 MiB worst-case memory budget allows, and
`WRITE_QUEUE_MAX` is a quarter of it for the eviction-safety reason already at that constant.
The retired derivation is quoted in place, because the number did not move when its reason
changed and someone will want to know why.

**N5 (P3) — the accounting expression, in both prose copies.**
`outstanding = accepted − applied − failed` → `− deferred`, in `writeq.rs`'s §Accounting and
in §J3's "`ledger_queued_lines` arithmetic, re-derived". The drift was inside the section
whose thesis is that there must be **one** expression which "cannot drift between them", and
the lesson is recorded beside the fix: a thesis does not enforce itself, the code
(`WriteQueueCounters::outstanding`, which was right) is the authority, and both sentences are
copies of it.

**N6 (P3) — "exact" scoped, and the offered code fix checked and declined.** The design doc's
"order among replayed intents is **exact** (`issued_ms`, `lane_seq`)" carried no scope where
every neighbouring claim carries one; it now carries the same one (**one agent's sequential
submissions**), with the mechanism written out. The review also offered a three-line closure —
move the clock read and the `fetch_add` inside the `receipts.lock()` `next_receipt` already
takes — and **it does not close the window**: that makes `(issued_ms, seq)` mutually monotone,
but the gap is *between* minting a receipt and reaching the `lanes.lock()` whose `push_back`
decides drain position, so a thread holding the lower `seq` can still be preempted and
enqueued second. Genuinely closing it means minting the receipt inside the
`graph.write() → lanes.lock()` nesting on the admission hot path; buying a documented ordering
scope with a new lock-order risk is the wrong trade, and the analysis is recorded at the
sentence so the next reader does not re-derive it.

**N7 (P3) — the prediction is labelled as one, and the alternative is argued.**
`IntentRecorded`'s docstring now opens with the tense ("durable as of the mutation log,
pending the close's final flush"), states that it is the only answer in the taxonomy asserting
a future rather than recording a past, and states why: `abort_workers` settles during the
quiesce and the final flush necessarily runs after it. Left as a prediction, deliberately —
settling only after the flush reports success means a close that cannot reach its store leaves
callers on `pending`, an *unsettled* answer that keeps waiters waiting, where they currently
get a specific one. The mitigation is recorded with it, including the elegant half: the
contradiction self-corrects from **both** directions after a restart (`restart_lost` if
nothing landed, `applied_after_restart` if the apply did), and the only observation window is a
`lambo_stats(receipt=…)` racing the close.

**F2 (P3) — the load-time skip that did not exist now exists, one layer up.**
`WRITE_INTENT_RETENTION` ended "and expired rows are **skipped at load**"; both adapters load
with an unfiltered `SELECT … WHERE session_id = ? ORDER BY issued_ms, lane_seq` and nothing
between there and the replay filtered by age, so a consumed row outliving the window answered
`applied_after_restart` where the same id in a process that had **not** restarted would have
been swept to `expired`. That is the asymmetry the `RECEIPT_RETENTION ==
WRITE_INTENT_RETENTION` assert exists to forbid, pointing the other way — so this was fixed
rather than documented away. The skip is at the replay's **seeding step**, not at the load:
one clock read in one place instead of a cutoff threaded through three adapters' load paths,
and it makes the stale row answer `restart_lost` — the honest analogue of `expired` for
another process's id. Unconsumed rows are seeded and replayed whatever their age: a debt does
not expire. The docstring now separates the two mechanisms it had conflated — *purging the
row* is lazy and clocked by the next consume (so a session that goes quiet keeps its rows, and
that is now said out loud), while *answering from the row* is bounded by the window. Pinned by
`a_consumed_intent_past_its_retention_window_is_not_answered_from`, which asserts both sides
of the boundary in one attach.

**F4 (P3) — the Cockroach cost stated at its real size, and the risk left standing.**
Verified at source: `consume_write_intent` issues **two** statements on Cockroach (the
`UPDATE`, then the retention `DELETE`), so a write costs **three** extra statements and not
two; and both intent mutations land in `plan_flush`'s `barrier` arm, whose first act is
`buckets.drain_into`, so each one **flushes the open bulk buckets** — two drains per write,
fragmenting the L82-1 batching in the transaction the close-time flush depends on. The real
figure is now in the design doc's as-built section, with the argument for leaving the purge on
the per-write path (it would remove one statement and none of the drains, and the seams cost
either an `apply_step` signature change on the flush hot path or a new trait method) and with
the B/Mooshik risk assessment: F4 sharpens the multiplier and does **not** move the
assessment, because the thing that matters is a number nobody has — absolute close-time
latency against a real serverless cluster. Bounded either way: a slow or failed close-time
flush loses nothing, which is what J3 bought.

**Register sweep, N3–N7 + F2 + F4 (per file, including the nulls).**

| File | Swept for | Result |
| --- | --- | --- |
| `src/writeq.rs` | every item-level stated reason naming a projection, a ceiling, a share, a bound or a floor — the sweep round 3's table claimed and did not finish | 12 sites corrected (2 production log lines, 10 docstrings/comments), each quoting what it used to say; plus the N4 decoupling, the N5 expression, the N7 tense and the F2 skip |
| `src/types/mod.rs` | `WRITE_INTENT_RETENTION`'s two mechanisms | the false "skipped at load" clause replaced by the two real mechanisms, named separately, with the lazy-purge consequence said out loud |
| `src/store/cockroach.rs`, `src/store/batch.rs` | the per-write statement count and the barrier behaviour | **nothing changed at the source** — both are correct as written; what was wrong was the *design doc's* count, now corrected there |
| `src/store/sqlite.rs` | the same | **nothing** — the identical two-statement consume, free on a local file, correctly documented at the function |
| `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` | the "exact" ordering claim, the unconsumed-row answer, the Cockroach cost | scope added with the declined-closure analysis; `pending` → `pending_replay`; the three-statements-and-two-drains figure with the risk assessment |
| this file | §J3's arithmetic paragraph | the fourth term added, with the drift's lesson recorded rather than quietly patched |
| `docs/reference/mcp.mdx`, `site/src/content/docs/mcp.mdx` | whether any user-facing text repeats a bound-from-a-rate claim or the three-term accounting | **nothing** — the round-3 pass had already rewritten the bounds and rates paragraphs correctly, and the accounting expression appears in neither mirror. Checked rather than assumed: `write_queue_outstanding` is described as a count, not a formula |

**The two items round 1 could not verify, settled.** The reviewer's second pass lost
filesystem access and listed both as residuals rather than clearing them. Both are now
checked by name, at the artifact.

* **The `mcp.mdx` byte-identity claim — TRUE, and about the right thing.** Diffing the two
  change bodies of `5ef7038` (`git show 5ef7038 -- <path> | grep '^[+-][^+-]'`, both mirrors)
  returns empty: the five rewritten passages are byte-identical between them. Worth stating
  what the claim never was: the two **files** are not identical and must not be. The site
  mirror carries `MdxNote`/`MdxWarning` Astro imports, site-relative link prefixes
  (`/lambo/config/#http-transport` against the docs site's `/config#http-transport`) and an
  extra "Verified clients" section. The invariant is that a *change* lands identically in
  both, and it held at round 3 and holds for all three of this remediation's mirror edits,
  each diffed the same way at commit time.
* **The −1 test-name set diff — TRUE, and nothing was silently dropped.** Extracted every
  `#[test]` / `#[tokio::test]` function name from the source at both revisions (name-level, so
  no build was needed: 1020 at `ed22476`, 1025 at `66f5aaa`) and set-differenced them. Five
  names disappear and ten appear, and **every disappearance is accounted for**:
  * The claimed merge, confirmed: `the_bounds_track_their_own_legs_between_the_clamps` and
    `the_measured_bound_is_clamped_at_both_ends` (two calibration-bound tests) are gone and
    `no_rate_can_move_the_bounds` is new — 2 out, 1 in, which is exactly the −1 in the
    cockroach count, and `writeq.rs` is a file that binary compiles.
  * The other three are **renames with their replacements present**:
    `embed_failure_degrades_to_fresh_concept` → `embed_failure_fails_the_write_and_writes_
    nothing` (the reversed pin), `a_burst_of_concepts_larger_than_the_probes_text_still_
    drains_at_a_clean_close` → `..._loses_nothing_at_a_clean_close`, and
    `one_agents_burst_never_outruns_its_own_lanes_drain_at_a_clean_close` →
    `one_agents_burst_never_loses_an_acked_write_at_a_clean_close`. All three renames follow
    the same substantive change: the claim being pinned stopped being about draining and
    started being about not losing.
  * No name vanishes without either a merge target or a rename target. The totals were
    consistent with the arithmetic; the names are what prove it, and that is what was owed.

**Dispositions, all fourteen.** Nothing carried to the integration pass: the operator's rule
is that a remediation round closes the P3s too.

| # | Sev | Disposition |
| --- | --- | --- |
| N1 | P1 | **Fixed.** Liveness gate + a structural (typed) failure classification; the prescribed `attempts` bound argued down; limits and describe() restated. Red-first, both directions. |
| N2 | P2 | **Fixed.** The timeout arm pinned on a paused clock. |
| N3 | P2 | **Fixed.** All twelve sites, both log lines included, each quoting what it said. |
| F3 | P2 | **Fixed as stated; behaviour argued and kept.** Boundary corrected at the source, availability consequence declared in both mirrors and the box. Turned up a further finding (`MatchStrategy`'s recall-only docstring), also fixed. |
| F5 | P2 (was an unconfirmed candidate) | **Confirmed real, graded, and fixed.** Not P1: the failure is loud at close and `serve` exits non-zero, so the invariant's clean-close precondition is violated rather than the invariant falsely satisfied. DDL-derived schema preflight at attach. |
| N4 | P3 | **Fixed.** The build-time coupling cut; both bounds now structural in magnitude as well as kind. |
| N5 | P3 | **Fixed** in both prose copies. |
| N6 | P3 | **Scoped**, and the offered code fix checked and declined with the analysis recorded. |
| N7 | P3 | **Labelled and argued** — the prediction is named as one; deferring the settle is worse and the reason is written down. |
| N8 | P3 | **Fixed** in the N1 commit: `pending_replay` is a distinct state, and part of N1's visibility answer. |
| N9 | P3 | **Fixed** in the N1 commit: five commits → six, in both narratives. |
| F1 | P3 | **Fixed** in the N1 commit: "Seven variants" at eleven, and the docstring now enumerates all of them. |
| F2 | P3 | **Fixed**, one layer up from where it was prescribed, plus the two conflated mechanisms separated. |
| F4 | P3 | **Stated at real size**, with the purge's placement argued and the B/Mooshik risk assessment written out. |

**Gates after the four remediation commits (repo-wide, all 15 result lines — never the lib
line alone):**

| Gate | At `ed03266` | After |
| --- | --- | --- |
| `cargo test --all --features fixtures` | 901 / 0 / 3 | **908 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 991 / 0 / 3 | **1000 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 563 / 0 / 0 | **565 / 0 / 0** |
| `scripts/observability/verify.sh` | 46 ok | **46 ok** |

Every delta is an addition; no test was removed or renamed by this remediation, so the
name-level check above needs no successor. `cargo fmt --all -- --check` clean;
`cargo clippy --all-targets -- -D warnings` clean on the default row, on
`store-sqlite,fixtures`, on `ship,fixtures` and on `--no-default-features
store-cockroach,embed-fixture`.

## J4 — Lease conflicts leave an artifact

A serve that loses the lease exits before it can open a ledger, so the most common
multi-agent failure is structurally invisible to I1 as specified. Two halves: a **pre-lease
startup line** written before the acquire attempt, and the **holder recording refused
takeovers**. Without these, metric 6 friction and every "why did this agent have no memory"
question stay unanswerable from artifacts.

J2 makes it cheaper, since a proxying serve is alive and can write its own lines.

### As-built (J4, 2026-08-22) — lease conflicts leave an artifact

Implemented on `wt/j4`. The single-writer lease is untouched; J4 is a
requirement placed on I1's existing ledger, not a second one, and every new line
rides the same [`Ledger::append`] path the `call`/`stats` lines ride. Nothing
existing changed shape — `scripts/observability/*` and `verify.sh` (still 46 ok,
`sample/calls.jsonl` byte-identical) parse exactly what they always did; the new
`startup` / `lease` / `completion` line kinds are additive and the kit ignores
unknown kinds by design.

**The pre-lease startup line.** `serve` now opens `Ledger::open` *before*
`resolve_role` makes its acquire attempt (it was opened after, on the holder
path, so a losing serve never reached it — the structural invisibility J4
exists to remove) and appends a `kind:"startup"` line (session, agent, transport,
`state:"acquiring"`). A serve about to lose the lease has therefore already left
an artifact. On the refused-exit path the ledger is **drained before the process
exits** — the startup/refused lines must not sit unflushed in the writer thread
and die with the process, or J4 would have left nothing at the very exit it was
written for.

**Both sides of a refusal.** When `resolve_role` is refused it appends a
`kind:"lease", event:"refused", side:"loser"` line (its own) naming the
incumbent, and persists the fact to the store via the new `record_lease_refusal`
(best-effort; `at` stamped by the store's clock, F18). The incumbent's serve
spawns a small recorder task (only with `--ledger`, only on the holder path)
that polls `pending_lease_refusals` for its own session, filters to refusals
against *its own* lease token, dedups by (refused_by, at), and appends a
`kind:"lease", event:"refused_takeover", side:"holder"` line. Together this is
the Done-when line: **"a refused lease acquisition appears in the ledger from
both sides"**. Persisting the fact in the store (not by reading the ledger on
the serve path — I1's rule) is what lets a *different* process, the holder,
write the second half.

**Proxy / degraded lines (J2 handoff).** A proxy is alive and now books its own
lines on the ledger it opened pre-lease: `kind:"lease", event:"proxying"` on a
successful first dial, and `kind:"lease", event:"proxying_stopped"` (with the
in-flight count) when the holder it forwarded to stops answering.

**Proof obligation 5 — the completion-line schema.** The ledger gains a
`kind:"completion"` line (agent, receipt, `state` ∈ applied /
applied_after_restart / failed / deferred) emitted from the write pipeline for
every durable write intent's lifecycle, on the same append path as every other
line. Applied lines carry `created_count` / `matched_count` — the I1 metric-2
fact set that a replayed durable intent previously hid because replay bypassed
the ordinary call path that builds `call`-line facts. This closes proof
obligation 5 of `J3-durability-redesign.md`.

*This paragraph also said it "restores the declared metric-2 regression", and
that was an **overclaim** — corrected by the JE2E-5 remediation (2026-08-22)
rather than left standing. J3 specified the repair as a schema change that
"moves `_ledger.py`, `dedup_rate.py`, `duplicates.py`, the observability README
and `verify.sh`"; J4 shipped the producer and moved none of the five, so at that
HEAD no consumer read a `completion` line, `dedup_rate.py` and `duplicates.py`
still saw nothing from MCP sessions, and the observability README still said in
the present tense that the completion line was **missing**. A producer with no
consumer restores no metric. The five are moved by the remediation, and the
completion join is now what makes an MCP-driven session's dedup rate a real
number — see [J E2E round-1 remediation](#j-e2e-round-1-remediation) below.*

**Deviation / disposition notes.**
* `lease_refusals` is a **new required table** in both migrations (it is part of
  the `INIT_SQL` both backends derive their preflight from, exactly like
  `write_intents`). An existing provisioned store must be re-provisioned
  (`lambo provision`) before this build preflights clean. This is deliberate and
  matches J3's `write_intents` precedent; it is a deployment action on the
  dogfood store, not a code path.
* The refusal poll is serve-level (500 ms), not in `Memory`: it needs the
  store, the session, the holder's own token and the ledger, all of which
  `serve()` holds; `Memory` stays ledger-agnostic except for the one
  `WriteCtx.ledger` the pipeline needs for completion lines.
* A refused loser still records its store refusal even when it degrades to a
  proxy (the acquire was refused; the holder should know it was contended).
* `verify.sh`/kit untouched; the two DDL-table-count test assertions moved from
  10 to 11 to account for the required `lease_refusals` table.

**Sweep — serve-startup ordering claims (the sweep J4 owed and did not run;
added by the JE2E-6 remediation, 2026-08-22).** J4 moved `Ledger::open` and its
startup line above `resolve_role` — a real ordering move, argued at the move
site — and its as-built shipped with **no sweep at all**, the only J task
section without one. J0's carried guidance is explicit that this family "lives
in more prose sites than any one of them signals", and J2 ran it twice; the
blast radius of J4's move landed in *other tasks'* files, which is exactly what
a per-task reviewer could not see. The `rg` was over `Ledger::open`, the arming
family (`handler is armed`, `armed before`, `first statement`), `resolve_role`,
`build_memory` and `pre-lease`: **85 hits, 5 stale.**

| Site | Claim | Verdict |
| --- | --- | --- |
| `ledger.rs` `Ledger::open` docstring | "serve calls this **after** the lease is taken and **after** the SIGTERM handler is armed (`shutdown_signal()` is the first statement once `resolve_role` returns; this call is the next one)" | **STALE, all three clauses — and the third generation at this site** (I-R2-1 corrected it, I-R3-1 corrected it again). Open is now pre-lease, pre-arming, and not "next" to anything. Rewritten with the ordering at HEAD *and* the availability argument re-made on true premises: a blocking open there strands **no** lease (better than the old claim), but the shutdown future has not been *created* yet, so the process keeps the default disposition — and the client sees a server that never answers `initialize`, the J2-L2 outcome |
| `ledger.rs` `opening_a_ledger_does_not_block…` test doc | "serve calls `Ledger::open` after the lease is taken, so an `open` on that path would hold the lease" | **STALE reason, live property.** The property is pinned unchanged and matters *more* pre-lease; the dead reason is quoted in place and replaced |
| `serve.rs` arming comment | enumerates "`Ledger::open`" among the startup work **below** the arming that it guards | **STALE, and it is the load-bearing R2-a claim.** `Ledger::open` and the startup append now run *above* `resolve_role`, unguarded. Harmless for a reason rather than by luck — open performs no I/O of its own, which `opening_a_ledger_does_not_block…` pins — but a stale enumeration credits the arming with covering work it does not cover, so the entry is removed and its absence explained |
| `serve.rs` `authorize_bind`'s "What J2 changed" | the pre-lease group "creates nothing and leaves nothing behind" | **FALSIFIED BY J4, found by this sweep and by no finding.** J4 put a third member in that group, and it is the first that *does* leave something behind: a ledger file and a line in it. A "What J4 changed" section now states the real claim — the group is about **retries, not traces**: refusing there takes no lease, and an append-only observability file the operator asked for blocks no retry. The `authorize_ledger`-above-the-open ordering is stated with it |
| `PHASE-8-surface.md:1767` | "The refusal runs as the *first statement* in `serve()`, before **`build_memory`**" | **STALE noun** — the one site J2-R1-7's tree-wide rewrite of that noun missed. The claim is true; the attach it names has been `resolve_role` since J2 |
| `serve_pre_handshake_durability.rs` module doc | the window it probes, and the proxy case's own sync point | re-read: **still true** — it claims the arming is before the transport handoff, which J4 did not move, and it enumerates nothing |
| `cli/serve_web.rs` `authorize_bind_web` | "mirrors `mcp::serve::authorize_bind`" — the rule, not the section | **still true**; a reader takes no lease, opens no ledger and binds no endpoint |
| `main.rs` `authorize_ledger` | keeps the CLI's wording verbatim | untouched |
| `PHASE-8-surface.md:1038, :1067` | `build_memory` in the module inventory and the `ResolvedBackends` note | clean — both are about the surviving library entry point, not about serve's ordering |
| `I-observability.md:294-319` | the I-R2-1 arming move | left alone, as J2 left it: narrative about I, not a claim about today |
| `src/mcp/serve.rs` `resolve_role` / `serve_builder` docs | the split's own ordering statements | re-read, **still true** — J4 moved work above them, not between them |

**Sweep — the "50 seconds by design" family (JE2E-7), run with the ordering one
because the two share a site.** J2-R2's register table asserted this family
"re-checked and still true"; it was not, which is why an E2E pass caught what
two J2 rounds did not. `rg` over `50 seconds`, `50s`,
`LEASE_TTL + ELECTION_SLACK`, `ELECTION_BUDGET`: **27 hits, 2 stale.**

| Site | Claim | Verdict |
| --- | --- | --- |
| `serve.rs` arming comment | "`resolve_role` is a loop that can legitimately run for `LEASE_TTL + ELECTION_SLACK` = **50 seconds**" | **STALE** — J2-L2 cut it to `ELECTION_BUDGET` = 20s. Restated at 20s and the argument **re-made** at the true number, which is the only thing that makes restating it worthwhile: 20s of deliberate deafness against a ~1.1 ms window in a process holding no lease and no tail is four orders of magnitude, and the 20s is not a rare worst case (the probe's dead-holder election took 10.2s) |
| `serve.rs` `shutdown_signal` docstring | "that loop is allowed to run for 50 seconds by design" | **STALE** — same correction, and its unguarded-work list now names the pre-lease group J4 added |
| `serve.rs` `ELECTION_BUDGET` / `ELECTION_SLACK` / `waiting_fits` docs | 20s, 15s, the [30,45] derivation | clean — these are where the true number was already written, in the same file, which is what made the two stale sites self-contradicting |
| `proxy.rs:168`, `proxy.rs:2254` | "worst case ≈ 50s" | clean — a **store** timeout (cockroach's 20s statement behind sqlx's 30s pool acquire), a different family that happens to share a number |
| `tests/serve_proxy_multi_client.rs:1122, :1172` | "the election waited 50s", "the pre-fix behaviour was 50s" | clean — past tense, accurate history of what J2-L2 measured and removed |

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

### As-built (J5, 2026-08-22)

* **The gate, landed FIRST (before the prose edits, per the catch).**
  `scripts/docs/check-mirror-drift.sh` + a `docs-mirror` job in `.github/workflows/ci.yml`,
  and the four mirror mdx paths + the script were added to CI's path filter so a
  mirror-only push still triggers it. It is **not** a raw byte `diff`: per the correction
  above (J2 round-1 review), the copies are deliberately not byte-identical, so the gate
  compares each pair's canonical shared-prose form — strips the Astro imports, drops
  mcp.mdx's site-only "Verified clients"/managed-CockroachDB section, and normalises the
  `/lambo/` link prefix and trailing slashes. Green on arrival, green after the edits.
* **The gate surfaced one genuine pre-existing drift:** site `cli.mdx` said the only
  `--scenario` was "v0.2", while the source (`src/cli/demo.rs`) and the reference copy
  both say v0.1. Reconciled to v0.1 inside the gate commit so the gate lands green.
* **The prose** (all four mirrors, byte-identical shared prose) documents HTTP as the
  default for a machine running more than one independent client — the reason is
  single-writer, not subagents (one orchestrator + its subagents is one connection, fine
  on stdio) — and the config-layering gotcha (transport touches every layer; a stale
  `command` beside a new `url` is rejected).
* **`--print-client-config <client>` — decided NOT built.** Emitting a paste-ready
  registration would need a client→registration-shape registry plus rig-specific operator
  paths (the pinned binary, config, session, ledger, and agent-ids of DOGFOOD-SETUP.md §4)
  that are DOGFOOD-rig configuration rather than lambo invariants, and the binary does not
  carry the resolved config path. A placeholder-template emit would be a half-verb, so it
  is documented here instead of scaffolded.

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
- [x] A refused lease acquisition appears in the ledger from both sides (J4) — a
      `kind:"startup"` line is written before the acquire; a refused serve writes its own
      `kind:"lease", event:"refused", side:"loser"` line and persists the fact to the store;
      the incumbent's recorder task writes `kind:"lease", event:"refused_takeover",
      side:"holder"`. Pinned by `refused_acquire_appears_in_the_ledger_from_both_sides`, which
      fails on pre-J4 code, plus the pre-lease/proxying/completion contract tests (§J4
      As-built). **Fails-on-pre-J4 evidence:** the both-sides and completion-schema tests
      require code that did not exist at the base `9eab99f` (a pre-lease `Ledger::open`, the
      `lease_refusals` store record, `WriteCtx.ledger`, the `completion`/`lease`/`startup`
      line builders) — on that base they could not even be written, and the observable
      contracts they assert (a loser+holder pair of lines, a completion line with
      created/matched counts) have no producer
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
      restart-lost answer distinctly, never "unknown" (J3) — eleven states, `expired` /
      `restart_lost` / `never_issued` all distinct, and `forbidden` for another agent's
      receipt (per-agent scoping, J1). Retrieval is `lambo_stats(receipt=…)`, **not an
      eighth tool** — deviation argued in §J3 Status. `restart_lost`'s wording is
      word-for-word consistent with the proxy's -32002
- [x] Waiting on a receipt restores read-your-writes for a caller that asks (J3) —
      `lambo_stats(receipt=…, wait_ms=…)`, clamped to `RECEIPT_WAIT_MAX` rather than
      refused, and exercised end to end through a **proxy** as well as in-process. A
      timed-out wait answers `pending`, which is honest rather than a failure
- [~] ~~The queue bound comes from a ceiling measured on the deployment's own embedder~~ —
      **this line's premise was retired at round 3, and the box is re-verified against the
      redesign's own truth.** Three successive estimator designs each abandoned acked writes
      at a clean close on a workload axis the estimate had not sampled (61/80 at round 1;
      37/68 at round 2; 326/361 and 13/16 at round 3, all at the release binary), so the
      durability invariant no longer rests on any measurement: **acked ⇒ (applied ∨ durable
      intent) at a clean close, by construction** — an accepted write is a durable intent
      from admission, a close defers what it cannot drain, and the next serve applies the
      remainder in order, idempotently per receipt id. Demonstrated at the release binary
      against the live BGE-M3 (16 agents × 4 × 1024 B: 64 acked == 1 applied-with-embedding
      + 63 durable intents, exactly; all 63 replayed; final store 64 embedded / 0 NULL / 0
      unconsumed — `evidence/mooshik-j3-durable-intents/`). Admission survives for fairness
      and memory only — `WRITE_QUEUE_LANE_MAX` (64, one agent's 1/16 share) and
      `WRITE_QUEUE_MAX` (1024, the receipt-store cap) — and being wrong there costs a
      refusal or a deferral, never a loss. Drops are `write_queue_dropped` beside seventeen
      other keys, with `write_queue_dropped_closed` separating a refused shutdown tail from
      real backpressure, `write_queue_deferred` counting close-deferred intents,
      `write_queue_replayed` counting a predecessor's intents applied at attach and
      `write_queue_replay_owed` the ones still owed; each drop
      says on its own receipt which cap refused it, as what it is (J3-R3-4). The embedder
      telemetry (probe + observed rates, `probe_optimism`, the flip line) is kept and gates
      nothing. **Tilde, and here are the honest limits of the redesign — its own, not the
      estimator's** (J3-R3-6's lesson: state each limit at its own magnitude): (1) **the
      crash window is unchanged** — a `kill -9` loses unflushed intents exactly as it loses
      the write-behind tail, up to one flush interval plus the in-flight batch; receipts
      answer `restart_lost`, honestly, when no durable record survived. (2) **Deferral is
      not application**: a deferred write lands only when a next serve of that session runs
      — a session nobody reopens holds its intents indefinitely (they are small and bounded
      by the queue cap, but "durable" is not "applied"), and its receipt store dies with the
      process, so the `intent_durable` answer outlives only the retention window of the
      NEXT process once it consumes. (3) **Cross-restart ordering is per-lane among
      replayed intents only**: a fresh write submitted after reattach can land before a
      replayed intent from the same agent (replay deliberately bypasses admission so a
      backlog cannot starve the fresh session — the same concurrent-submission scope
      §Ordering declares). (4) **A replay failure consumes the intent as `failed` only when
      the failure is about the write itself** — an embedder that answers and refuses *this
      content* settles it, mirroring the in-session worker, and the receipt says so for one
      retention window. That is the only class that is destroyed, and the boundary is a
      type, not a guess: `LamboError::Embed`. Everything else leaves the intent durable and
      stops the replay — the embedder unreachable or timed out, the store failing, the lease
      moved — because those say nothing about the write and a later serve may be in a
      position to apply it. **Stated at its own magnitude, which is what round 1 caught this
      line failing to do** (J3-R3-6's lesson, missed once here): before that fix the
      magnitude was not one poison record but *the entire backlog*, settled `failed` at one
      `HYBRID_IO_TIMEOUT` each, on any embedder outage overlapping one attach; now a replay
      does not begin at all until one liveness embed has been answered, so an outage costs
      one embed and no intents. What remains, honestly: a *permanently* unreachable embedder
      leaves the intents owed forever — visible as `write_queue_replay_owed` and as
      `pending_replay` on every affected receipt, and preferred over destroying acked writes
      on a dependency's silence. (5) **A crash blocks replay for the rest of the lease
      TTL.** After a `kill -9` the next `serve` is refused for the remaining 30–45 s of the
      lease (the J2 arithmetic two limits above), so "the next serve replays them" is not
      "the next serve *attempt*": every receipt in that window answers `restart_lost` and
      the replay happens only at the attempt that wins the lapsed lease. (6) **On the
      default configuration a write needs the embedder, every time** (round-1 F3). The
      honesty fix's stated boundary — "the capability-absent arm stays a degrade" — is about
      the **store**: `vector_ok` is `VECTOR_SEARCH`, SQLite advertises it unconditionally,
      `build_embedder` always yields an embedder or a startup error, and `hybrid` is the
      config default, so **no arm degrades on a missing or dead embedder** and an
      unreachable llama.cpp fails every `lambo_derive` while it is unreachable. That is the
      intended trade — the alternative is the silent `embedding: NULL` write it replaced,
      and no acked write dies with the outage *unattempted*: a write not yet attempted
      when the session closes waits for an embedder as a durable intent, while a write the
      worker reaches during the outage fails, and its receipt says so — the honest
      asymmetry (J3-R2R-2) — but it is an *availability* consequence and it
      was stated nowhere a user reads. It is now in both `mcp.mdx` mirrors together with
      the way to opt out, which is to **declare** the degraded mode
      (`match_strategy = "canonical"`, keyword-only, session-wide) rather than receive it
      one call at a time. The behaviour is deliberately unchanged: a per-call failure
      cannot be promoted to a session-uniform degrade, because "not reached, and will not be
      for this session" is not distinguishable at the protocol from "not reached, for
      200 ms" — N1's classification separates *refused* from *not reached*, and no
      classification can separate *transient* from *permanent*. None of the six is a loss on
      the clean path, which is what this box exists to exclude; all six are reasons not to
      tick it flat
- [x] One agent's writes apply in submission order, pinning the `Temporal` chain (J3) — and
      with two agents interleaving through one process, the §13 conflict sentence's `writer`
      is **measured** rather than assumed: J1 made the same-instant collision path
      non-degenerate (J1-R1-8). Satisfied as the amendment requires, by filtering the
      session-wide chain on `agent_id`
      (`interleaved_agents_each_keep_their_own_order_on_the_temporal_chain`, which also
      asserts the chain *actually* interleaves or the filter would prove nothing). Stronger
      than specified in one respect and it is worth stating, **scoped as J3-R1-10 scoped
      it**: the interaction is opened on the call path, so for writes one agent sends
      *sequentially* chain order is submission order by construction and cannot be corrupted
      by an out-of-order drain at all. Two calls that agent has in flight **simultaneously**
      are outside the claim — `begin_interaction_as` and `admit`'s lane lock are two critical
      sections with no ordering between them across threads — and the scoped statement lives
      at `writeq`'s §Ordering, in `derive_async_as`, in the tool instructions and in both
      `mcp.mdx` mirrors. This line is ~470 lines from that scoping, which is why it repeats
      it rather than pointing at it (J3-R2-6). Per-agent FIFO lanes are still enforced in
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
