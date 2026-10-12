# #98 / #99: say what a concept is, and land the agent_id rule at the point of use

Bounded wording change for 0.6.0. No API redesign, no refusal, no soft
signal on id shape.

## What changed

- `lambo_derive` description: a concept is one fact, stated atomically and
  short enough that a later restatement matches it, such as a decision with
  its reason; one good and one bad example; activity goes to
  `lambo_record_action`, and a decision links to its facts with `parent_of`.
- `WireConcept.content` doc: one fact, a sentence, not a paragraph or a log.
- `lambo_record_action` description: this is where activity goes, a short
  line naming the act; a fact or decision to recall is a concept.
- Every `agent_id` parameter doc (seven identical, plus `lambo_reserve`'s):
  one stable id naming your model or client, same on every call, never a
  per-task or per-role id, with good and bad examples and the reason.
- Server instructions: both rules, with the reason for the id rule.
- Write acks (derive, derive_image, record_action) end with
  `Recorded under agent_id "<id>"; send this same id on every call.` and
  carry `agent_id` in `structuredContent`. Additive: no key removed or
  renamed, and the id is the caller's own, already checked by
  `caller_agent` (one line, at most 256 characters).
- `tool_schemas.golden.json` updated for the description changes only.
- `docs/reference/mcp.mdx` and the site mirror; repo `AGENTS.md` example ids
  (`claude-orchestrator` was itself a role-suffixed id).

## Not done

- The before/after measurement both issues ask for needs more dogfooding on
  a build with this wording, from a read-only ledger copy. Left for the
  issues' follow-up.
- No soft signal for task-shaped ids, and no derive-receipt length note:
  both are design choices the issues defer until that measurement.
- `~/lambo-dogfood` hooks are out of scope for this lane.
