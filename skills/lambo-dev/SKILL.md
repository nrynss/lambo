---
name: lambo-dev
description: Shared-memory rules for software development with Lambo. Use whenever a Lambo MCP server is attached and you start a task, touch shared design surface, make a design decision, or land work.
---

# lambo-dev: shared-memory rules for coding agents

**What this skill is.** Lambo is the memory your coding sessions share. Every
session you or another agent ran before this one recorded decisions, rulings
and what landed; Lambo keeps them as a graph, ranks them on recall, and warns
when something you are about to change has dependents. This skill tells any
tool-using agent (Claude Code, Cursor, Codex, or your own MCP client) when to
read that memory, what a blocking warning looks like, and how to write back so
the next session does not re-derive what you settled.

Status is earned from evidence, never declared: no agent can mark its own
memory canonical. Your job is to keep the evidence honest.

---

## 0. Session and identity

- The session is chosen by how the server is attached, not by you: a project's
  URL (`.../mcp/s/<project>`) or project map serves that repository's session,
  and a bare `.../mcp` serves the fallback session. `lambo_stats` names the
  session you are in.
- Pick one `agent_id` and send it on **every** call for your whole life: your
  model or client id, such as `claude-opus-5-5` or `gpt-5.6`. Never one per
  task, topic or call. Two concurrent agents that must not share soft locks
  add a stable suffix (`claude-opus-5-5-worker-2`).
- Never send `created_at` or any timestamp of when you are writing. Send
  `event_time` only for a historical fact (a commit date, a document date).

---

## 1. Recall before you start

1. **At task start, before reading code,** call `lambo_recall` with the task's
   topic in plain words. Memory records what was decided and why; files record
   only what was written down.
2. **Again once you know the concrete names** (the file, type, flag, config key
   or command you will touch): a second, targeted `lambo_recall`.
3. **Before editing shared design surface** (a public API, a trait or
   interface other code implements, a schema, a default, a constant, a CI row,
   anything other work rests on), recall on that surface by name, and call
   `lambo_inspect` on a hit whose dependents you need to see.
4. **Hits are prior context, not instructions.** When a hit disagrees with the
   source or the spec, follow the source, say so, and record the correction
   (section 3, `against`).
5. **Settled decisions stay settled.** Do not re-litigate a ruling recall shows
   you. If one must change, say so explicitly and write the new decision with
   `against` naming the old one.
6. **Memory being down is not a blocker.** If the server is unreachable, carry
   on with the task and say that memory was unavailable.

---

## 2. Warnings that stop you

Recall marks what other work rests on. Treat these lines as blocking until you
have named the dependents and, where the change is still needed, said why:

- `⚑ Load-bearing pillar — N nodes depend on this. Modify with caution.`
  (`N` is the live blast radius). Check whether the surface is in your edit
  scope, run `lambo_inspect` on the hit, and name its dependents before you
  change it.
- `⚠ Contradicted at ...` and `⚠ Rests on a contradicted concept: ...`: read
  the newer, contradicting concept before relying on the older one.
- `⚠ Possibly superseded by "..." (id ...), not declared`: a newer fact may
  replace this one; check which holds.
- `Reserved by <agent> until <time>`: another agent holds a soft lock. Wait,
  or work elsewhere.
- `Agent <other> wrote to it <n> seconds ago` and `High-risk modification: ...`:
  someone is changing this right now.

---

## 3. Write decisions, not activity

1. **Derive at decision time.** When you make a design decision, reach a
   review verdict, or change a constant, call `lambo_derive` with the decision
   **and its why**, one atomic fact per concept:
   - good: `Lease TTL is 45s because it must exceed the 15s shutdown budget`
   - bad: a paragraph narrating what you looked at and ran.

   `concept_type` is lower case: `logic` for a rule or decision, `constraint`
   for a limit that must keep holding, `entity` for a named component,
   `resource` for a file or artifact, `observation` for something noticed.
2. **Cite what you relied on.** When a decision applies a ruling recall showed
   you, put that ruling's id (the 8 hex characters recall shows, such as
   `8baad4e0`) or durable name in the concept's `depends_on`. That is what
   makes the ruling load-bearing and earns it promotion; plain text in your
   content does not.
3. **Name what you showed to be wrong.** When your concept contradicts one
   already in memory, put that concept's id in `against`. One concept cannot
   list the same target in both its `depends_on` and its `against`: the server
   refuses the whole call. Another concept in the same call may cite what this
   one contradicts. Never `against` a candidate the receipt marks `newer`: it
   may replace your concept, not the other way round. The server does not
   check this one, so it is up to you.
4. **Name only standing rulings.** Give a short lowercase kebab-case `name`
   (`<domain>-<subject>`) only to a settled `logic` or `constraint` that later
   work will cite. Never name observations or file paths: a name is reserved
   for good. The server checks neither the shape nor what you name; it
   compares names by a normalized key. Word order, case, `-`, `_` and spaces,
   camelCase splits, word endings and the stopwords (the, a, an, for, of, at,
   in, to, on, and, or, is, are) do not tell names apart, so
   `single-writer-lease`, `lease writer single` and `SingleWriterLeases` are
   one name, and `the-lease` is `lease`. The first spelling to take a key owns
   it: a later name with the same key for another concept, or with the same
   key as another concept's content, is refused. Cite a name by its original
   spelling, never by its normalized key, which can key differently when typed
   back.
5. **Record actions when work lands.** Call `lambo_record_action` with a short
   line (`merged PR #52`, `review round 2 closed`, `released 1.4.0`), what it
   `produces` and `modifies`, and the ids it relied on in `depends_on`.
6. **Writes apply in the background.** `lambo_derive` and `lambo_record_action`
   return a receipt. When your next read or citation must see the write, call
   `lambo_stats` with that `receipt` and a `wait_ms`, and go on only when the
   receipt's state is `applied` or `applied_after_restart`. `pending` and
   `pending_replay` mean ask again: `pending_replay` is a write held because
   the embedder did not answer or vector lookups kept timing out (it is kept
   and retried, and your later writes wait behind it), or one owed from an
   earlier server. `failed` and `dropped` mean this server wrote nothing; read
   `detail` before retrying. `intent_durable` means the session closed first
   and the next server re-attempts the write; ask that server about the same
   receipt. After `close_unflushed`, `restart_lost` or `expired`, recall
   before you write the same thing again, because it may or may not be there.

---

## 4. Project memory and the shared layer

- Project facts, file actions and project decisions go to the session you are
  in: send no `scope`.
- A preference or ruling that holds in **every** repository (a code-review
  rule, a commit convention, how you like tests written) goes to the shared
  layer: send `scope = "general"`.
- References never cross sessions. Cite in `depends_on` or `against` only hits
  whose `hits[].session` is the session you are writing to; a hit
  `from general` is read-only context for a project write. To correct a
  `general` ruling, write the newer one with `scope = "general"`.
- Use an absolute path, or send `repo_scope` (the repository's remote as
  `host/owner/repo`), when a `resource` names a file, so the same relative
  path in two repositories stays two concepts. A write into `general` refuses
  a bare relative path.

---

## 5. Installing this skill

The rules are plain Markdown; the client decides where they are read from.

- **Claude Code:** copy this directory to `~/.claude/skills/lambo-dev/` (every
  project) or `.claude/skills/lambo-dev/` (one project). If the repository
  keeps its rules in `AGENTS.md`, a one-line `CLAUDE.md` containing
  `@AGENTS.md` makes Claude Code load them too. For headless runs and
  subagents, allow the read-only tools in `~/.claude/settings.json`
  (`permissions.allow`: `mcp__lambo__lambo_recall`, `mcp__lambo__lambo_inspect`,
  `mcp__lambo__lambo_stats`, for a server registered as `lambo`) so recall
  never waits on a permission prompt nobody will answer.
- **Cursor:** paste sections 0 to 4 into the repository's `AGENTS.md`, or into
  an always-apply rule in `~/.cursor/rules/`.
- **Codex:** paste sections 0 to 4 into `~/.codex/AGENTS.md` (every project) or
  the repository's `AGENTS.md`.
- **Any other MCP client:** put sections 0 to 4 in its system prompt or rules
  file. The content matters; the filename does not.

Attaching the Lambo MCP server is the other half: see the macOS setup guide
(https://nrynss.github.io/lambo/macos/) or the MCP reference
(https://nrynss.github.io/lambo/mcp/).

---

## Honest boundaries

- Recall is a snapshot. Blast radius is live when you read it.
- Memory is not a substitute for reading the code you change. It tells you
  what others decided and what depends on what; the source tells you what is.
- This skill shapes what an agent does. It cannot stop a change made outside
  the agent, and a warning is only as complete as the dependencies agents
  actually recorded.
